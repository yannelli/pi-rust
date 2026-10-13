//! Session creation (upstream `sdk.ts` `createAgentSession`): restore or choose the model and
//! thinking level, select tools, and wire the agent to the session.

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};

use pi_agent::{Agent, SharedTool, StreamFn};
use pi_ai::models::clamp_thinking_level;
use pi_ai::{Model, ThinkingLevel};
use tokio::sync::watch;

use super::auth::auth_stream_fn;
use super::services::AgentSessionServices;
use super::tools::{ToolState, default_tool_names, prompt_options};
use super::{AgentSession, RunState, lock};
use crate::app::format_no_models_available_message;
use crate::model_resolver::{InitialModelOptions, ModelCatalog, ScopedModel, find_initial_model};
use crate::session::SessionManager;
use crate::settings::DEFAULT_THINKING_LEVEL;
use crate::settings::tools::{apply_tool_modifiers, is_tool_modifier, matches_any_tool, tool_list_error};

/// `--no-tools` (`All`) or `--no-builtin-tools` (`Builtin`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoTools {
    All,
    Builtin,
}

/// Inputs of [`create_agent_session`]. Unset fields come from settings and the session.
#[derive(Clone)]
pub struct CreateAgentSessionOptions {
    pub services: AgentSessionServices,
    pub session_manager: SessionManager,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub scoped_models: Vec<ScopedModel>,
    /// Tool names or `*` patterns; a list of only `+name`/`-name` entries changes the defaults.
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Vec<String>,
    pub no_tools: Option<NoTools>,
    /// SDK tools registered next to the built-in ones (active unless filtered).
    pub custom_tools: Vec<SharedTool>,
    /// Model request function in place of `pi_ai::stream`; auth and settings still apply.
    pub stream_fn: Option<StreamFn>,
}

impl CreateAgentSessionOptions {
    pub fn new(services: AgentSessionServices, session_manager: SessionManager) -> Self {
        Self {
            services,
            session_manager,
            model: None,
            thinking_level: None,
            scoped_models: Vec::new(),
            tools: None,
            exclude_tools: Vec::new(),
            no_tools: None,
            custom_tools: Vec::new(),
            stream_fn: None,
        }
    }
}

pub struct CreateAgentSessionResult {
    pub session: Arc<AgentSession>,
    /// Why the restored or saved model was not used.
    pub model_fallback_message: Option<String>,
}

struct ToolSelection {
    active: Vec<String>,
    allowed: Option<Vec<String>>,
    uses_default_tools: bool,
    modifiers: Vec<String>,
}

fn select_tools(options: &CreateAgentSessionOptions) -> anyhow::Result<ToolSelection> {
    if let Some(error) = options.tools.as_deref().and_then(tool_list_error) {
        anyhow::bail!("Invalid tools option: {error}");
    }
    let settings = &options.services.settings;
    let defaults = if options.no_tools.is_some() { Vec::new() } else { default_tool_names(settings, &[]) };
    let modifiers = options.tools.clone().filter(|tools| tools.iter().any(|entry| is_tool_modifier(entry)));
    let selected = match &modifiers {
        Some(modifiers) => Some(apply_tool_modifiers(&defaults, modifiers)),
        None => options.tools.clone(),
    };
    let allowed = match &modifiers {
        Some(_) if options.no_tools == Some(NoTools::All) => selected.clone(),
        Some(_) => None,
        None => options.tools.clone().or_else(|| (options.no_tools == Some(NoTools::All)).then(Vec::new)),
    };
    let mut active = selected.unwrap_or(defaults);
    active.retain(|name| !matches_any_tool(&options.exclude_tools, name));
    Ok(ToolSelection {
        active,
        allowed,
        uses_default_tools: (options.tools.is_none() || modifiers.is_some()) && options.no_tools.is_none(),
        modifiers: modifiers.unwrap_or_default(),
    })
}

/// The model to use: the given one, the session's (when it still has auth), or the default.
fn choose_model(options: &CreateAgentSessionOptions, has_existing: bool) -> (Option<Model>, Option<String>) {
    let runtime = options.services.model_runtime.as_ref();
    let settings = &options.services.settings;
    let mut model = options.model.clone();
    let mut fallback = None;
    let restored = options.session_manager.build_session_context().model;
    if model.is_none()
        && has_existing
        && let Some((provider, id)) = restored
    {
        model = runtime.find_model(&provider, &id).filter(|model| runtime.has_configured_auth(&model.provider));
        if model.is_none() {
            fallback = Some(format!("Could not restore model {provider}/{id}"));
        }
    }
    if model.is_none() {
        let levels = settings.all_model_thinking_levels();
        let (provider, id) = (settings.default_provider(), settings.default_model());
        let initial = InitialModelOptions {
            is_continuing: has_existing,
            default_provider: provider.as_deref(),
            default_model_id: id.as_deref(),
            default_thinking_level: settings.default_thinking_level(),
            model_thinking_levels: Some(&levels),
            ..InitialModelOptions::default()
        };
        model = find_initial_model(&initial, runtime).ok().and_then(|result| result.model);
        fallback = match (&model, fallback) {
            (None, _) => Some(format_no_models_available_message()),
            (Some(found), Some(message)) => Some(format!("{message}. Using {}/{}", found.provider, found.id)),
            (Some(_), None) => None,
        };
    }
    (model, fallback)
}

fn choose_thinking_level(
    options: &CreateAgentSessionOptions,
    model: Option<&Model>,
    has_existing: bool,
    restored: Option<ThinkingLevel>,
) -> ThinkingLevel {
    let settings = &options.services.settings;
    let mut thinking = options.thinking_level;
    if thinking.is_none() && has_existing {
        thinking = Some(restored.or(settings.default_thinking_level()).unwrap_or(DEFAULT_THINKING_LEVEL));
    }
    if thinking.is_none()
        && let Some(model) = model
    {
        thinking = settings.model_thinking_level(&model.provider, &model.id);
    }
    let thinking = thinking.or(settings.default_thinking_level()).unwrap_or(DEFAULT_THINKING_LEVEL);
    model.map_or(ThinkingLevel::Off, |model| clamp_thinking_level(model, thinking))
}

/// Create a session over `options.session_manager`. A new session records its model and
/// thinking level first so a resume restores them.
pub async fn create_agent_session(options: CreateAgentSessionOptions) -> anyhow::Result<CreateAgentSessionResult> {
    let services = options.services.clone();
    let settings = services.settings.clone();
    let existing = options.session_manager.build_session_context();
    let has_existing = !existing.messages.is_empty();
    let has_thinking_entry =
        options.session_manager.get_branch(None).iter().any(|entry| entry.kind() == "thinking_level_change");
    let (model, model_fallback_message) = choose_model(&options, has_existing);
    let restored = existing.thinking_level.filter(|_| has_thinking_entry);
    let thinking = choose_thinking_level(&options, model.as_ref(), has_existing, restored);
    let selection = select_tools(&options)?;

    let mut session_manager = options.session_manager;
    if has_existing {
        if !has_thinking_entry {
            session_manager.append_thinking_level_change(thinking)?;
        }
    } else {
        if let Some(model) = &model {
            session_manager.append_model_change(&model.provider, &model.id)?;
        }
        session_manager.append_thinking_level_change(thinking)?;
    }

    let agent = Agent::new(model.clone().unwrap_or_default());
    agent.update_state(|state| {
        state.system_prompt = None;
        state.thinking_level = thinking;
        state.messages = existing.messages;
    });
    agent.set_queue_modes(settings.steering_mode(), settings.follow_up_mode());
    let has_custom_stream = options.stream_fn.is_some();
    let stream_fn = auth_stream_fn(services.model_runtime.clone(), settings.clone(), options.stream_fn);
    let resources = services.resources().resources().clone();
    let mut active = selection.active;
    for tool in &options.custom_tools {
        if !active.iter().any(|name| name == tool.name()) {
            active.push(tool.name().to_string());
        }
    }
    let base_prompt = prompt_options(&services.cwd, &resources, &active);
    let tools = ToolState {
        registry: Vec::new(),
        custom: options.custom_tools,
        active,
        allowed: selection.allowed,
        excluded: options.exclude_tools,
        uses_default_tools: selection.uses_default_tools,
        default_tool_modifiers: selection.modifiers,
        base_prompt,
    };
    let (idle, _) = watch::channel(true);
    let session = Arc::new_cyclic(|weak| AgentSession {
        agent: agent.clone(),
        services: services.clone(),
        session: Mutex::new(session_manager),
        listeners: Mutex::new(Vec::new()),
        next_listener: AtomicU64::new(1),
        state: Mutex::new(RunState::default()),
        idle,
        tools: Mutex::new(tools),
        stream_fn,
        has_custom_stream,
        model_selected: AtomicBool::new(model.is_some()),
        scoped_models: Mutex::new(options.scoped_models),
        agent_listener: Mutex::new(None),
        disposed: AtomicBool::new(false),
        weak: weak.clone(),
    });
    session.refresh_builtin_tools();
    let active = session.active_tool_names();
    session.set_active_tools_by_name(&active);
    session.install_hooks();
    let weak = Arc::downgrade(&session);
    let id = agent.subscribe(move |event| {
        if let Some(session) = weak.upgrade() {
            session.on_agent_event(event);
        }
    });
    *lock(&session.agent_listener) = Some(id);
    session.refresh_environment();
    Ok(CreateAgentSessionResult { session, model_fallback_message })
}
