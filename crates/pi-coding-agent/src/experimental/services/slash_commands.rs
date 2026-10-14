//! The process-local `SlashCommands` registry and the built-in commands (upstream `services/slash-commands.ts`
//! and `slash-commands-provider.ts`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use pi_ai::ThinkingLevel;
use pi_chord::facets::{Facet, define_facet};
use pi_chord::services::{Service, define_local_service};
use pi_chord::{BoxFuture, ChordError, ChordResult, Context, Json};
use regex::Regex;

use super::agent_controller::{AGENT_CONTROLLER, AgentCompactionRequest, AgentController, AgentControllerClient};
use super::agent_controller::{AgentOperationResponse, AgentQueueResponse};
use super::models::{MODELS, ModelRef, ModelSummary, Models, ModelsClient};
use super::plugins::{PRESENTATION_PLUGINS, PresentationPlugins, PresentationPluginsClient, SESSION_PLUGINS};
use super::plugins::{SessionPlugins, SessionPluginsClient};
use super::presentation_ui::{PRESENTATION_UI, PresentationSelectItem, PresentationUi};
use crate::experimental::process::lock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashCommandCompletion {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SlashCommandRunResult {
    Operation(AgentOperationResponse),
    Queue(AgentQueueResponse),
}

pub type SlashCommandRun =
    Arc<dyn Fn(String, Context) -> BoxFuture<ChordResult<Option<SlashCommandRunResult>>> + Send + Sync>;
pub type SlashCommandCompleter = Arc<dyn Fn(&str) -> Option<Vec<SlashCommandCompletion>> + Send + Sync>;

#[derive(Clone)]
pub struct SlashCommandContribution {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub get_argument_completions: Option<SlashCommandCompleter>,
    pub run: SlashCommandRun,
}

impl SlashCommandContribution {
    pub fn new<F, Fut>(name: impl Into<String>, run: F) -> Self
    where
        F: Fn(String, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChordResult<Option<SlashCommandRunResult>>> + Send + 'static,
    {
        let run: SlashCommandRun = Arc::new(move |args, context| Box::pin(run(args, context)));
        Self { name: name.into(), description: None, argument_hint: None, get_argument_completions: None, run }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_argument_hint(mut self, hint: impl Into<String>) -> Self {
        self.argument_hint = Some(hint.into());
        self
    }

    /// Whether both contributions run the same callback.
    pub fn same_run(&self, other: &SlashCommandContribution) -> bool {
        Arc::ptr_eq(&self.run, &other.run)
    }
}

pub type SlashCommandsListener = Arc<dyn Fn(&[SlashCommandContribution]) + Send + Sync>;

struct Entry {
    command: SlashCommandContribution,
    closed: AtomicBool,
}

#[derive(Default)]
struct RegistryState {
    commands: Vec<(String, Vec<Arc<Entry>>)>,
    listeners: Vec<(u64, SlashCommandsListener)>,
    next_listener: u64,
}

/// A registration; `close` is idempotent.
pub struct SlashCommandRegistration(Box<dyn Fn() + Send + Sync>);

impl SlashCommandRegistration {
    pub fn close(&self) {
        (self.0)();
    }

    /// For `env.own`: close when the owning facet deactivates.
    pub fn into_disposal(self) -> impl FnOnce() -> ChordResult<()> + Send + 'static {
        move || {
            self.close();
            Ok(())
        }
    }
}

static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9:-]*$").expect("valid pattern"));

/// Process-local slash command contributions. Same-name replacements are staged until earlier ones retire.
#[derive(Clone, Default)]
pub struct SlashCommandRegistry(Arc<Mutex<RegistryState>>);

pub static SLASH_COMMANDS: LazyLock<Service<SlashCommandRegistry>> =
    LazyLock::new(|| define_local_service("pi.local.slash-commands").expect("valid service ID"));

impl SlashCommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, command: SlashCommandContribution) -> ChordResult<SlashCommandRegistration> {
        validate(&command)?;
        if lock(&self.0).commands.iter().any(|(name, _)| name == &command.name) {
            return Err(ChordError::error(format!("Slash command /{} is already registered", command.name)));
        }
        Ok(self.add(command))
    }

    /// Stage a same-name replacement while the earlier facet generation retires.
    pub fn replace(&self, command: SlashCommandContribution) -> ChordResult<SlashCommandRegistration> {
        validate(&command)?;
        Ok(self.add(command))
    }

    pub fn list(&self) -> Vec<SlashCommandContribution> {
        list(&lock(&self.0))
    }

    pub fn subscribe(&self, listener: SlashCommandsListener) -> impl Fn() + Send + Sync + use<> {
        let (id, commands) = {
            let mut state = lock(&self.0);
            state.next_listener += 1;
            let id = state.next_listener;
            state.listeners.push((id, listener.clone()));
            (id, list(&state))
        };
        listener(&commands);
        let state = Arc::downgrade(&self.0);
        move || {
            if let Some(state) = state.upgrade() {
                lock(&state).listeners.retain(|(existing, _)| *existing != id);
            }
        }
    }

    fn add(&self, command: SlashCommandContribution) -> SlashCommandRegistration {
        let name = command.name.clone();
        let entry = Arc::new(Entry { command, closed: AtomicBool::new(false) });
        let first = {
            let mut state = lock(&self.0);
            match state.commands.iter_mut().find(|(existing, _)| existing == &name) {
                Some((_, entries)) => {
                    entries.push(entry.clone());
                    false
                }
                None => {
                    state.commands.push((name.clone(), vec![entry.clone()]));
                    true
                }
            }
        };
        if first {
            self.publish();
        }
        let registry = Arc::downgrade(&self.0);
        SlashCommandRegistration(Box::new(move || {
            if entry.closed.swap(true, Ordering::SeqCst) {
                return;
            }
            let Some(state) = registry.upgrade() else { return };
            {
                let mut state = lock(&state);
                let Some(index) = state.commands.iter().position(|(existing, _)| existing == &name) else { return };
                let entries = &mut state.commands[index].1;
                if !entries.first().is_some_and(|head| Arc::ptr_eq(head, &entry)) {
                    return;
                }
                while entries.first().is_some_and(|head| head.closed.load(Ordering::SeqCst)) {
                    entries.remove(0);
                }
                if entries.is_empty() {
                    state.commands.remove(index);
                }
            }
            SlashCommandRegistry(state).publish();
        }))
    }

    fn publish(&self) {
        let (commands, listeners) = {
            let state = lock(&self.0);
            (list(&state), state.listeners.iter().map(|(_, listener)| listener.clone()).collect::<Vec<_>>())
        };
        for listener in listeners {
            listener(&commands);
        }
    }
}

fn list(state: &RegistryState) -> Vec<SlashCommandContribution> {
    state.commands.iter().filter_map(|(_, entries)| entries.first().map(|entry| entry.command.clone())).collect()
}

fn validate(command: &SlashCommandContribution) -> ChordResult<()> {
    if NAME.is_match(&command.name) {
        Ok(())
    } else {
        Err(ChordError::type_error(format!("Invalid slash command name: {}", command.name)))
    }
}

pub fn create_slash_commands_runtime_facet(registry: SlashCommandRegistry) -> Arc<dyn Facet> {
    define_facet("@pi/slash-commands-runtime", move |env| {
        env.provide_local(&SLASH_COMMANDS, Arc::new(registry.clone()))
    })
}

pub type ReloadPresentationPlugins = Arc<dyn Fn(Json) -> BoxFuture<ChordResult<()>> + Send + Sync>;

pub fn create_built_in_slash_commands_facet(reload_presentation_plugins: ReloadPresentationPlugins) -> Arc<dyn Facet> {
    define_facet("@pi/slash-commands-builtin", move |env| {
        let commands = env.use_local(&SLASH_COMMANDS)?;
        let models = env.use_service(&MODELS)?;
        let controller = env.use_service(&AGENT_CONTROLLER)?;
        let ui = env.use_local(&PRESENTATION_UI)?;
        let presentation = env.use_service(&PRESENTATION_PLUGINS)?;
        let session = env.use_service(&SESSION_PLUGINS)?;
        let (activation_env, reload) = (env.clone(), reload_presentation_plugins.clone());
        env.on_activate(move || {
            let (env, commands, ui) = (activation_env.clone(), commands.clone(), ui.clone());
            let (models, controller) = (ModelsClient(models.clone()), AgentControllerClient(controller.clone()));
            let plugins = (PresentationPluginsClient(presentation.clone()), SessionPluginsClient(session.clone()));
            let reload = reload.clone();
            async move {
                let (commands, ui) = (commands.get()?, ui.get()?);
                for command in [
                    model_command(models.clone(), ui.clone()),
                    thinking_command(models, ui.clone()),
                    compact_command(controller, ui.clone()),
                    reload_command(plugins, ui, reload),
                ] {
                    env.own(commands.replace(command)?.into_disposal())?;
                }
                Ok(())
            }
        })
    })
}

fn reload_command(
    (presentation, session): (PresentationPluginsClient, SessionPluginsClient),
    ui: Arc<dyn PresentationUi>,
    reload: ReloadPresentationPlugins,
) -> SlashCommandContribution {
    SlashCommandContribution::new("reload", move |_args, context| {
        let (presentation, session, ui, reload) = (presentation.clone(), session.clone(), ui.clone(), reload.clone());
        async move {
            ui.show_status("Reloading plugins…", &context);
            let data = presentation.reload(&context).await?;
            session.reload(&context).await?;
            reload(data).await?;
            ui.show_status("Reloaded plugins.", &context);
            Ok(None)
        }
    })
    .with_description("Reload server-selected plugins")
}

fn model_key(model: &ModelSummary) -> String {
    format!("{}/{}", model.provider, model.model_id)
}

fn model_command(models: ModelsClient, ui: Arc<dyn PresentationUi>) -> SlashCommandContribution {
    let completions = models.clone();
    let mut command = SlashCommandContribution::new("model", move |args, context| {
        let (models, ui) = (models.clone(), ui.clone());
        async move {
            let state = models.state()?.ok_or_else(|| ChordError::error("Models service is not ready"))?;
            let available = &state.catalog.available_models;
            let mut selected = exact_model(available, &args);
            if !args.is_empty() && selected.is_none() {
                return Err(ChordError::error(format!("Unknown model: {args}")));
            }
            if selected.is_none() {
                let current =
                    state.configuration.model.as_ref().map(|model| format!("{}/{}", model.provider, model.model_id));
                let items = available
                    .iter()
                    .map(|model| {
                        let key = model_key(model);
                        let label = if current.as_deref() == Some(key.as_str()) {
                            format!("{} (selected)", model.name)
                        } else {
                            model.name.clone()
                        };
                        PresentationSelectItem { value: key.clone(), label, description: Some(key) }
                    })
                    .collect();
                let Some(value) = ui.select("Select model:", items, current, &context).await? else { return Ok(None) };
                selected = exact_model(available, &value);
                if selected.is_none() {
                    return Err(ChordError::error(format!("Unknown model: {value}")));
                }
            }
            let Some(selected) = selected else { return Ok(None) };
            let model = ModelRef { provider: selected.provider.clone(), model_id: selected.model_id.clone() };
            models.select(model, &context).await?;
            ui.show_status(&format!("Selected {}.", model_key(&selected)), &context);
            Ok(None)
        }
    })
    .with_description("Select model")
    .with_argument_hint("<provider/model>");
    command.get_argument_completions = Some(Arc::new(move |prefix| {
        let normalized = prefix.to_lowercase();
        let state = completions.state().ok().flatten()?;
        let matches = state
            .catalog
            .available_models
            .iter()
            .filter(|model| format!("{} {}", model_key(model), model.name).to_lowercase().contains(&normalized));
        let completion = |model: &ModelSummary| SlashCommandCompletion {
            value: model_key(model),
            label: model.model_id.clone(),
            description: Some(model.provider.clone()),
        };
        Some(matches.map(completion).collect())
    }));
    command
}

fn thinking_description(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "No reasoning",
        ThinkingLevel::Minimal => "Very brief reasoning",
        ThinkingLevel::Low => "Light reasoning",
        ThinkingLevel::Medium => "Moderate reasoning",
        ThinkingLevel::High => "Deep reasoning",
        ThinkingLevel::Xhigh => "Extra-high reasoning",
        ThinkingLevel::Max => "Maximum reasoning",
    }
}

fn thinking_command(models: ModelsClient, ui: Arc<dyn PresentationUi>) -> SlashCommandContribution {
    SlashCommandContribution::new("thinking", move |args, context| {
        let (models, ui) = (models.clone(), ui.clone());
        async move {
            let levels = models.get_thinking_levels(&context).await?;
            let mut selected = levels.iter().copied().find(|level| level.as_str() == args.to_lowercase());
            if !args.is_empty() && selected.is_none() {
                let names: Vec<&str> = levels.iter().map(|level| level.as_str()).collect();
                let message = format!("Unknown thinking level \"{args}\". Available levels: {}.", names.join(", "));
                return Err(ChordError::error(message));
            }
            if selected.is_none() {
                let current = models.state()?.map(|state| state.configuration.thinking_level);
                let item = |level: &ThinkingLevel| PresentationSelectItem {
                    value: level.as_str().into(),
                    label: if current == Some(*level) {
                        format!("{} (selected)", level.as_str())
                    } else {
                        level.as_str().into()
                    },
                    description: Some(thinking_description(*level).into()),
                };
                let items = levels.iter().map(item).collect();
                let current = current.map(|level| level.as_str().to_string());
                let Some(value) = ui.select("Select thinking level:", items, current, &context).await? else {
                    return Ok(None);
                };
                selected = levels.iter().copied().find(|level| level.as_str() == value);
                if selected.is_none() {
                    return Err(ChordError::error(format!("Unknown thinking level: {value}")));
                }
            }
            let selected = selected.unwrap_or_default();
            models.select_thinking(selected, &context).await?;
            ui.show_status(&format!("Thinking level: {}.", selected.as_str()), &context);
            Ok(None)
        }
    })
    .with_description("Set thinking level")
    .with_argument_hint("<level>")
}

fn compact_command(controller: AgentControllerClient, ui: Arc<dyn PresentationUi>) -> SlashCommandContribution {
    SlashCommandContribution::new("compact", move |args, context| {
        let (controller, ui) = (controller.clone(), ui.clone());
        async move {
            ui.show_status("Compacting…", &context);
            let custom_instructions = (!args.is_empty()).then_some(args);
            let response = controller.compact(AgentCompactionRequest { custom_instructions }, &context).await?;
            Ok(Some(SlashCommandRunResult::Operation(response)))
        }
    })
    .with_description("Manually compact the session context")
    .with_argument_hint("<instructions>")
}

fn exact_model(models: &[ModelSummary], query: &str) -> Option<ModelSummary> {
    if query.is_empty() {
        return None;
    }
    let normalized = query.to_lowercase();
    let matches: Vec<&ModelSummary> = models
        .iter()
        .filter(|model| model_key(model).to_lowercase() == normalized || model.model_id.to_lowercase() == normalized)
        .collect();
    (matches.len() == 1).then(|| matches[0].clone())
}
