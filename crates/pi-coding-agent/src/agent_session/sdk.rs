//! Session creation (upstream `sdk.ts` `createAgentSession`): restore or choose the model and
//! thinking level, select tools, and wire the agent to the session.

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};

use pi_agent::{Agent, SharedTool, StreamFn};
use pi_ai::models::clamp_thinking_level;
use pi_ai::{Model, ThinkingLevel};
use tokio::sync::watch;

use super::auth::auth_stream_fn;
use super::builtins::{BuiltinState, McpSessionOptions};
use super::cache::SessionWarmerHost;
use super::services::AgentSessionServices;
use super::tools::{ActiveSelection, ToolSelection, ToolState, builtin_tools, default_tool_names, prompt_options};
use super::{AgentSession, RunState, lock};
use crate::app::format_no_models_available_message;
use crate::cache_warmer::CacheWarmer;
use crate::model_resolver::{InitialModelOptions, ModelCatalog, ScopedModel, find_initial_model};
use crate::session::SessionManager;
use crate::settings::DEFAULT_THINKING_LEVEL;
use crate::settings::tools::{apply_tool_modifiers, is_tool_modifier, matches_any_tool, tool_list_error};
use crate::tools::ToolsOptions;
use crate::virtual_models::get_branch_selection;

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
    /// Fast mode for this session; unset reads `fastMode` from settings.
    pub fast: Option<bool>,
    pub scoped_models: Vec<ScopedModel>,
    /// Tool names or `*` patterns; a list of only `+name`/`-name` entries changes the defaults.
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Vec<String>,
    pub no_tools: Option<NoTools>,
    /// SDK tools registered next to the built-in ones (active unless filtered).
    pub custom_tools: Vec<SharedTool>,
    /// Model request function in place of `pi_ai::stream`; auth and settings still apply.
    pub stream_fn: Option<StreamFn>,
    /// How the built-in MCP support loads its config and connects (tests, SDK hosts).
    pub mcp: McpSessionOptions,
    /// Extensions loaded for this session ([`crate::extensions::load_extensions`]).
    pub extensions: Option<Arc<crate::extensions::ExtensionRunner>>,
    /// The `session_start` event the extensions get when they bind; unset sends `startup`.
    pub session_start_event: Option<serde_json::Value>,
    /// Options of the built-in tools: custom backends, the bash spawn hook, and whether the
    /// shell tools expose the session's `PI_*` variables. Unset uses
    /// [`ToolsOptions::from_settings`]; start from it to keep the settings.
    pub tool_options: Option<ToolsOptions>,
    /// `--max-turns` and `--max-cost` (pi-rust).
    pub run_limits: super::RunLimits,
}

impl CreateAgentSessionOptions {
    pub fn new(services: AgentSessionServices, session_manager: SessionManager) -> Self {
        Self {
            services,
            session_manager,
            model: None,
            thinking_level: None,
            fast: None,
            scoped_models: Vec::new(),
            tools: None,
            exclude_tools: Vec::new(),
            no_tools: None,
            custom_tools: Vec::new(),
            stream_fn: None,
            mcp: McpSessionOptions::default(),
            extensions: None,
            session_start_event: None,
            tool_options: None,
            run_limits: super::RunLimits::default(),
        }
    }
}

pub struct CreateAgentSessionResult {
    pub session: Arc<AgentSession>,
    /// The extensions attached to the session (upstream `extensionsResult`).
    pub extensions: Option<Arc<crate::extensions::ExtensionRunner>>,
    /// Why the restored or saved model was not used.
    pub model_fallback_message: Option<String>,
}

fn select_tools(options: &CreateAgentSessionOptions) -> anyhow::Result<(Vec<String>, ToolSelection)> {
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
    let selection = ToolSelection {
        allowed,
        excluded: options.exclude_tools.clone(),
        uses_default_tools: (options.tools.is_none() || modifiers.is_some()) && options.no_tools.is_none(),
        default_tool_modifiers: modifiers.unwrap_or_default(),
    };
    Ok((active, selection))
}

/// The model to use: the given one, the session's (when it still has auth), or the default.
fn choose_model(options: &CreateAgentSessionOptions, has_existing: bool) -> (Option<Model>, Option<String>) {
    let runtime = options.services.model_runtime.as_ref();
    let settings = &options.services.settings;
    let mut model = options.model.clone();
    let mut fallback = None;
    // Assistant messages name the physical model that answered; a virtual selection is only in
    // `model_change` entries.
    let get_model = |provider: &str, id: &str| runtime.find_model(provider, id);
    let restored = get_branch_selection(&options.session_manager.get_branch(None), &get_model);
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
    let (active, selection) = select_tools(&options)?;
    let (selection_uses_defaults, default_modifiers) =
        (selection.uses_default_tools, selection.default_tool_modifiers.clone());

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
        state.fast = options.fast.unwrap_or_else(|| settings.fast_mode());
        state.messages = existing.messages;
    });
    agent.set_queue_modes(settings.steering_mode(), settings.follow_up_mode());
    let has_custom_stream = options.stream_fn.is_some();
    let request_hooks = super::RequestHooksSlot::default();
    let stream_fn =
        auth_stream_fn(services.model_runtime.clone(), settings.clone(), options.stream_fn, request_hooks.clone());
    let resources = services.resources().resources().clone();
    let base_prompt = prompt_options(&services.cwd, &resources);
    let mut mcp_options = options.mcp;
    if let Some(runner) = &options.extensions {
        mcp_options.registry = Some(runner.mcp_registry().clone());
    }
    let builtins = BuiltinState { mcp_options, ..BuiltinState::default() };
    let (idle, _) = watch::channel(true);
    let session = Arc::new_cyclic(|weak| {
        let tool_options = options.tool_options;
        let builtin = builtin_tools(&services.cwd, &settings, tool_options.as_ref(), weak.clone());
        let mut tools = ToolState::new(builtin, options.custom_tools, selection, base_prompt);
        tools.tool_options = tool_options;
        let warmer_host = Arc::new(SessionWarmerHost::new(weak.clone()));
        AgentSession {
            agent: agent.clone(),
            services: services.clone(),
            session: Mutex::new(session_manager),
            listeners: Mutex::new(Vec::new()),
            next_listener: AtomicU64::new(1),
            state: Mutex::new(RunState::default()),
            idle,
            tools: Mutex::new(tools),
            tool_authorizer: Mutex::new(None),
            builtins: Mutex::new(builtins),
            stream_fn,
            has_custom_stream,
            model_selected: AtomicBool::new(model.is_some()),
            scoped_models: Mutex::new(options.scoped_models),
            agent_listener: Mutex::new(None),
            disposed: AtomicBool::new(false),
            cache_warmer: CacheWarmer::new(warmer_host.clone()),
            warmer_host,
            extensions: Default::default(),
            run_limits: super::run_limits::RunLimitState::new(options.run_limits),
            request_hooks,
            file_checkpoints: Default::default(),
            weak: weak.clone(),
        }
    });
    session.install_builtin_tools();
    session.refresh_tools(ActiveSelection { names: Some(active), include_all_registered: true });
    // A resumed session that uses the default tools gets the loadout its transcript records.
    if selection_uses_defaults {
        session.restore_tools_from_transcript(&default_modifiers);
    }
    session.install_hooks();
    let weak = Arc::downgrade(&session);
    let id = agent.subscribe(move |event| {
        if let Some(session) = weak.upgrade() {
            session.on_agent_event(event);
        }
    });
    *lock(&session.agent_listener) = Some(id);
    if let Some(event) = options.session_start_event {
        session.set_session_start_event(event);
    }
    let extensions = options.extensions;
    if let Some(runner) = extensions.clone() {
        session.attach_extensions(runner).await;
    }
    session.start_builtin_extensions();
    Ok(CreateAgentSessionResult { session, extensions, model_fallback_message })
}
