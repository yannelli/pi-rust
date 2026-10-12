//! A coding session: the agent with its tools and prompt, settings, credentials, model
//! selection, auto retry, session persistence, and automatic compaction.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};

use pi_agent::{Agent, AgentEvent, AgentHooks, StreamFn};
use pi_ai::auth::{
    AuthResolver, Credential, CredentialStore, FileCredentialStore, OAuthLookup, RuntimeCredentialStore,
};
use pi_ai::models::clamp_thinking_level;
use pi_ai::{ContentBlock, Model, ModelRegistry, ThinkingLevel};
use tokio_util::sync::CancellationToken;

use crate::compaction::{
    self, CompactionResult, CompactionSettings, context_tokens, prepare_compaction, should_compact,
};
use crate::config::{ContextFile, auth_path, load_context_files, models_path};
use crate::model_resolver::ScopedModel;
use crate::session::{SessionManager, session_dir};
use crate::settings::{SettingsManager, SettingsOptions};
use crate::system_prompt::{SystemPromptOptions, build_system_prompt};
use crate::tools;

#[path = "app_auth.rs"]
mod auth;
#[path = "app_events.rs"]
mod events;
#[path = "app_retry.rs"]
mod retry;
#[path = "app_setup.rs"]
mod setup;

pub use events::SessionEvent;
pub use retry::{is_retryable_assistant_error, retry_delay_ms};
pub use setup::{
    StartupError, configured_providers, format_no_api_key_found_message, format_no_models_available_message,
    provider_login_help, resolve_prompt_input,
};

/// Receives session events; see [`CodingSession::subscribe`].
pub type SessionListener = Arc<dyn Fn(&SessionEvent<'_>) + Send + Sync>;

#[derive(Clone, Default)]
pub struct SessionOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    /// `--provider`; needs `model`.
    pub provider: Option<String>,
    /// `--model` pattern, optionally with a `:<thinking>` suffix.
    pub model: Option<String>,
    /// `--api-key`: a runtime key for the selected model's provider, never written to disk.
    pub api_key: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    /// `--models` patterns; `None` uses the `enabledModels` setting.
    pub models: Option<Vec<String>>,
    /// `--system-prompt`: text, or a file whose contents replace the default prompt.
    pub system_prompt: Option<String>,
    /// `--append-system-prompt` texts or files, joined with blank lines.
    pub append_system_prompt: Vec<String>,
    /// `--tools`; `None` uses the `defaultTools` setting.
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Vec<String>,
    pub no_tools: bool,
    pub no_builtin_tools: bool,
    pub no_context_files: bool,
    /// Session file to open.
    pub session_file: Option<PathBuf>,
    pub continue_recent: bool,
    pub persist: bool,
    pub session_dir: Option<PathBuf>,
    /// Settings to use in place of `<agent dir>/settings.json` and `<cwd>/.pi/settings.json`.
    pub settings: Option<Arc<SettingsManager>>,
    /// Credential store to use in place of `<agent dir>/auth.json`.
    pub credentials: Option<Arc<dyn CredentialStore>>,
    /// OAuth providers by id; `None` means no OAuth refresh.
    pub oauth: Option<OAuthLookup>,
    /// Model request function in place of `pi_ai::stream`; auth and settings still apply.
    pub stream_fn: Option<StreamFn>,
}

pub struct CodingSession {
    pub agent: Agent,
    session: Arc<Mutex<SessionManager>>,
    registry: ModelRegistry,
    settings: Arc<SettingsManager>,
    resolver: Arc<AuthResolver>,
    shared: Arc<retry::Shared>,
    pub compaction: CompactionSettings,
    cwd: PathBuf,
    sessions_dir: Option<PathBuf>,
    pub context_files: Vec<ContextFile>,
    pub tool_names: Vec<String>,
    scoped_models: Vec<ScopedModel>,
    /// Why the restored or saved model was not used (upstream `modelFallbackMessage`).
    pub model_fallback_message: Option<String>,
    /// Startup warnings from model resolution and `--models` patterns.
    pub warnings: Vec<String>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl CodingSession {
    pub async fn new(options: SessionOptions) -> anyhow::Result<Self> {
        let settings = options.settings.clone().unwrap_or_else(|| {
            Arc::new(SettingsManager::create(&options.cwd, &options.agent_dir, SettingsOptions::default()))
        });
        let mut registry = ModelRegistry::load(&models_path(&options.agent_dir))?;
        let store: Arc<dyn CredentialStore> = match &options.credentials {
            Some(store) => store.clone(),
            None => Arc::new(FileCredentialStore::new(auth_path(&options.agent_dir))),
        };
        let runtime = Arc::new(RuntimeCredentialStore::new(store));
        let oauth = options.oauth.clone().unwrap_or_else(|| Arc::new(|_: &str| None));
        let resolver = Arc::new(AuthResolver::new(runtime.clone(), oauth));
        resolver.set_provider_configs(registry.auth_configs());
        let mut configured = setup::configured_providers(&resolver, &registry).await;
        registry.set_configured_providers(configured.clone());

        let sessions_dir = options
            .persist
            .then(|| options.session_dir.clone().unwrap_or_else(|| session_dir(&options.agent_dir, &options.cwd)));
        let session = match (&options.session_file, options.continue_recent, &sessions_dir) {
            (Some(path), _, _) => SessionManager::open(path)?,
            (None, true, Some(dir)) => match SessionManager::most_recent(dir) {
                Some(path) => SessionManager::open(&path)?,
                None => SessionManager::create(&options.cwd, Some(dir)),
            },
            (None, _, dir) => SessionManager::create(&options.cwd, dir.as_deref()),
        };
        let restored = session.context();
        let has_existing_session = !restored.messages.is_empty();
        let inputs = setup::CliInputs {
            provider: options.provider.as_deref(),
            model: options.model.as_deref(),
            thinking: options.thinking,
            models: options.models.as_deref(),
            has_existing_session,
        };
        let cli = setup::choose_cli_model(&inputs, &settings, &registry);
        let mut errors = cli.errors.clone();
        if let Some(key) = &options.api_key {
            match &cli.model {
                Some(model) => {
                    runtime.set_runtime_api_key(&model.provider, key.clone());
                    configured.insert(model.provider.clone());
                    registry.set_configured_providers(configured);
                }
                None => errors.push(
                    "--api-key requires a model to be specified via --model, --provider/--model, or --models".into(),
                ),
            }
        }
        if !errors.is_empty() {
            return Err(StartupError { errors, warnings: cli.warnings }.into());
        }
        let restored_selection = setup::RestoredSelection {
            has_existing_session,
            model: restored.model.as_ref(),
            thinking: restored.thinking_level,
        };
        let choice = setup::choose_session_model(&cli, &restored_selection, &settings, &registry);
        let Some(model) = choice.model else {
            return Err(
                StartupError { errors: vec![format_no_models_available_message()], warnings: cli.warnings }.into()
            );
        };
        let thinking = choice.thinking;

        let no_tools = options.no_tools || options.no_builtin_tools;
        let tool_names = setup::choose_tools(&settings, options.tools.as_deref(), &options.exclude_tools, no_tools);
        let agent_tools = setup::build_tools(&tool_names, &options.cwd, &settings)?;
        let context_files =
            if options.no_context_files { Vec::new() } else { load_context_files(&options.cwd, &options.agent_dir) };
        let custom_prompt =
            options.system_prompt.as_deref().and_then(|input| resolve_prompt_input(input, &options.cwd));
        let appended: Vec<String> =
            options.append_system_prompt.iter().filter_map(|input| resolve_prompt_input(input, &options.cwd)).collect();
        let append_prompt = (!appended.is_empty()).then(|| appended.join("\n\n"));
        let system_prompt = build_system_prompt(&SystemPromptOptions {
            custom_prompt: custom_prompt.as_deref(),
            append_prompt: append_prompt.as_deref(),
            selected_tools: &tool_names,
            context_files: &context_files,
            cwd: &options.cwd,
        });

        let agent = Agent::new(model.clone());
        agent.update_state(|state| {
            state.system_prompt = Some(system_prompt);
            state.thinking_level = thinking;
            state.tools = agent_tools;
            state.messages = restored.messages.clone();
        });
        agent.set_queue_modes(settings.steering_mode(), settings.follow_up_mode());
        agent.set_hooks(AgentHooks {
            stream_fn: Some(auth::auth_stream_fn(resolver.clone(), settings.clone(), options.stream_fn.clone())),
            convert_to_llm: Some(setup::block_images_hook(settings.clone())),
            session_id: Some(session.id().to_string()),
            ..AgentHooks::default()
        });

        let shared = Arc::new(retry::Shared::new(settings.clone(), model.context_window));
        let events = shared.clone();
        agent.subscribe(move |event| events.on_agent_event(event));
        let session = Arc::new(Mutex::new(session));
        let recorder = session.clone();
        agent.subscribe(move |event| {
            if let AgentEvent::MessageEnd { message } = event
                && let Err(error) = lock(&recorder).append_message(message)
            {
                eprintln!("Warning: could not save session: {error:#}");
            }
        });
        let coding = Self {
            agent,
            session,
            registry,
            compaction: setup::compaction_settings(&settings, &model),
            settings,
            resolver,
            shared,
            cwd: options.cwd,
            sessions_dir,
            context_files,
            tool_names,
            scoped_models: cli.scoped,
            model_fallback_message: choice.fallback_message,
            warnings: cli.warnings,
        };
        {
            let mut manager = lock(&coding.session);
            if restored.model.as_ref() != Some(&(model.provider.clone(), model.id.clone())) {
                manager.append_model_change(&model.provider, &model.id)?;
            }
            if restored.thinking_level != Some(thinking) {
                manager.append_thinking_level_change(thinking)?;
            }
        }
        coding.refresh_environment();
        Ok(coding)
    }

    /// Publish the session, model, and reasoning level to bash commands as `PI_*` variables.
    fn refresh_environment(&self) {
        let state = self.agent.state();
        let mut variables = BTreeMap::new();
        variables.insert("PI_SESSION_ID".to_string(), self.session_id());
        if let Some(file) = self.session_file() {
            variables.insert("PI_SESSION_FILE".to_string(), file.display().to_string());
        }
        variables.insert("PI_PROVIDER".to_string(), state.model.provider.clone());
        variables.insert("PI_MODEL".to_string(), state.model.id.clone());
        variables.insert("PI_REASONING_LEVEL".to_string(), state.thinking_level.as_str().to_string());
        tools::set_session_environment(variables);
    }

    pub fn registry(&self) -> &ModelRegistry {
        &self.registry
    }

    pub fn settings(&self) -> &Arc<SettingsManager> {
        &self.settings
    }

    pub fn auth_resolver(&self) -> &Arc<AuthResolver> {
        &self.resolver
    }

    /// Models for cycling, from `--models` or `enabledModels`.
    pub fn scoped_models(&self) -> &[ScopedModel] {
        &self.scoped_models
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn model(&self) -> Model {
        self.agent.state().model
    }

    pub fn session_file(&self) -> Option<PathBuf> {
        lock(&self.session).file().map(Path::to_path_buf)
    }

    pub fn session_id(&self) -> String {
        lock(&self.session).id().to_string()
    }

    pub fn sessions_dir(&self) -> Option<&Path> {
        self.sessions_dir.as_deref()
    }

    /// The API key a request to `provider` would use now, refreshing OAuth tokens if needed.
    pub async fn api_key_for(&self, provider: &str) -> Option<String> {
        let resolved = self.resolver.resolve(provider, &Default::default()).await.ok().flatten();
        resolved.and_then(|resolved| resolved.auth.api_key)
    }

    /// Receive session events until [`Self::unsubscribe`].
    pub fn subscribe(&self, listener: impl Fn(&SessionEvent<'_>) + Send + Sync + 'static) -> u64 {
        let id = self.shared.next_listener.fetch_add(1, Ordering::Relaxed);
        lock(&self.shared.listeners).push((id, Arc::new(listener)));
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        lock(&self.shared.listeners).retain(|(listener, _)| *listener != id);
    }

    /// Switch models. The thinking level becomes the model's `modelThinkingLevels` entry, the
    /// `defaultThinkingLevel` setting, or the current level, clamped to what the model supports.
    pub fn set_model(&self, model: Model) -> anyhow::Result<()> {
        if !self.registry.has_api_key(&model.provider) {
            anyhow::bail!("No API key for {}/{}", model.provider, model.id);
        }
        let current = self.agent.state().thinking_level;
        let level = self
            .settings
            .model_thinking_level(&model.provider, &model.id)
            .or(self.settings.default_thinking_level())
            .unwrap_or(current);
        lock(&self.session).append_model_change(&model.provider, &model.id)?;
        self.shared.context_window.store(model.context_window, Ordering::Relaxed);
        self.agent.update_state(|state| state.model = model);
        self.set_thinking_level(level)
    }

    /// Set the thinking level, clamped to the current model; recorded when it changes.
    pub fn set_thinking_level(&self, level: ThinkingLevel) -> anyhow::Result<()> {
        let level = clamp_thinking_level(&self.model(), level);
        let previous = self.agent.state().thinking_level;
        self.agent.update_state(|state| state.thinking_level = level);
        if level != previous {
            lock(&self.session).append_thinking_level_change(level)?;
        }
        self.refresh_environment();
        Ok(())
    }

    /// Start a fresh session with the same model, tools, and prompt.
    pub fn new_session(&self) -> anyhow::Result<()> {
        let model = self.model();
        let thinking = self.agent.state().thinking_level;
        let mut session = SessionManager::create(&self.cwd, self.sessions_dir.as_deref());
        session.append_model_change(&model.provider, &model.id)?;
        session.append_thinking_level_change(thinking)?;
        self.agent.update_hooks(|hooks| hooks.session_id = Some(session.id().to_string()));
        *lock(&self.session) = session;
        self.agent.update_state(|state| state.messages.clear());
        self.refresh_environment();
        Ok(())
    }

    /// Switch to the session stored in `path`.
    pub fn open_session(&self, path: &Path) -> anyhow::Result<()> {
        let session = SessionManager::open(path)?;
        let context = session.context();
        if let Some(model) = context.model.as_ref().and_then(|(provider, id)| self.registry.find(provider, id)) {
            let model = model.clone();
            self.shared.context_window.store(model.context_window, Ordering::Relaxed);
            self.agent.update_state(|state| state.model = model);
        }
        if let Some(level) = context.thinking_level {
            let level = clamp_thinking_level(&self.model(), level);
            self.agent.update_state(|state| state.thinking_level = level);
        }
        self.agent.update_hooks(|hooks| hooks.session_id = Some(session.id().to_string()));
        *lock(&self.session) = session;
        self.agent.update_state(|state| state.messages = context.messages);
        self.refresh_environment();
        Ok(())
    }

    /// Run a prompt. Compacts first when the context is near the window, retries transient
    /// provider errors per `retry.*`, and compacts once more when the provider reports overflow.
    pub async fn prompt(&self, text: &str, images: Vec<ContentBlock>) -> anyhow::Result<()> {
        self.ensure_auth().await?;
        {
            let mut retry = lock(&self.shared.retry);
            retry.abort_requested = false;
            retry.last_assistant = None;
        }
        self.compact_if_needed().await;
        self.agent.prompt(text, images).await?;
        self.after_run().await
    }

    async fn ensure_auth(&self) -> anyhow::Result<()> {
        let provider = self.model().provider;
        if self.registry.has_api_key(&provider) || matches!(self.resolver.check(&provider).await, Ok(Some(_))) {
            return Ok(());
        }
        let stored = self.resolver.store().read(&provider).await.ok().flatten();
        Err(anyhow::anyhow!(match stored {
            Some(Credential::OAuth(_)) => setup::oauth_failed_message(&provider),
            _ => format_no_api_key_found_message(&provider),
        }))
    }

    async fn compact_if_needed(&self) {
        let state = self.agent.state();
        if should_compact(context_tokens(&state.messages), state.model.context_window, &self.compaction)
            && let Err(error) = self.compact(None).await
        {
            eprintln!("Warning: automatic compaction failed: {error:#}");
        }
    }

    /// Summarize older history now. `instructions` focuses the summary.
    pub async fn compact(&self, instructions: Option<&str>) -> anyhow::Result<CompactionResult> {
        let plan = prepare_compaction(&lock(&self.session), &self.compaction)
            .ok_or_else(|| anyhow::anyhow!("Nothing to compact yet"))?;
        let request = auth::request_auth(&self.resolver, &self.model()).await.map_err(anyhow::Error::msg)?;
        let mut model = request.model;
        model.headers.extend(request.auth.headers);
        let mut session = lock(&self.session).clone();
        let result = compaction::compact(
            &mut session,
            plan,
            &model,
            request.auth.api_key,
            instructions,
            &self.compaction,
            CancellationToken::new(),
        )
        .await?;
        let messages = session.context().messages;
        *lock(&self.session) = session;
        self.agent.update_state(|state| state.messages = messages);
        Ok(result)
    }
}
