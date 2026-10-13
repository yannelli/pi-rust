//! CLI wiring of the session runtime (upstream `main.ts` runtime factory): project trust,
//! services, the CLI model choice, and `--api-key` for every session the runtime creates.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use pi_agent::{SharedTool, StreamFn};
use pi_ai::ThinkingLevel;
use pi_ai::auth::{CredentialStore, OAuthLookup};

use crate::agent_session::{
    AgentSessionRuntime, CreateAgentSessionOptions, CreateServicesOptions, McpSessionOptions, NoTools, ResourceOptions,
    RuntimeDiagnostic, RuntimeFactory, RuntimeParts, RuntimeTarget, create_agent_session,
    create_agent_session_services, lock,
};
use crate::cli::Args;
use crate::config::home_dir;
use crate::resources::paths::{is_local_path, resolve_path};
use crate::resources::trust::{
    DefaultProjectTrust, ProjectTrustRequest, ProjectTrustResolution, ProjectTrustStore, resolve_project_trust,
};
use crate::session::SessionManager;
use crate::settings::{SettingsManager, SettingsOptions};

#[path = "app_setup.rs"]
mod setup;

pub use setup::{
    configured_providers, format_no_api_key_found_message, format_no_models_available_message, oauth_failed_message,
    provider_login_help,
};

/// What the command line asks of every session the runtime creates.
#[derive(Clone, Default)]
pub struct AppOptions {
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
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Vec<String>,
    pub no_tools: Option<NoTools>,
    pub resources: ResourceOptions,
    /// `--approve` (`Some(true)`) or `--no-approve` (`Some(false)`).
    pub project_trust_override: Option<bool>,
    /// Settings for every runtime in place of the settings files.
    pub settings: Option<Arc<SettingsManager>>,
    /// Credential store in place of `auth.json`.
    pub credentials: Option<Arc<dyn CredentialStore>>,
    /// OAuth providers by id; `None` uses the built-in ones.
    pub oauth: Option<OAuthLookup>,
    /// Model request function in place of `pi_ai::stream`.
    pub stream_fn: Option<StreamFn>,
    pub custom_tools: Vec<SharedTool>,
    /// How sessions run the built-in MCP support.
    pub mcp: McpSessionOptions,
}

fn resolve_cli_paths(cwd: &Path, paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|value| if is_local_path(value) { resolve_path(value, cwd).display().to_string() } else { value.clone() })
        .collect()
}

/// A `--system-prompt` value: an existing file relative to `cwd` becomes its path.
fn resolve_prompt_arg(input: &str, cwd: &Path) -> String {
    let path = cwd.join(input);
    if !input.is_empty() && path.exists() { path.display().to_string() } else { input.to_string() }
}

impl AppOptions {
    /// Options from parsed arguments; local paths resolve against `cwd`.
    pub fn from_args(args: &Args, cwd: &Path, agent_dir: &Path) -> Self {
        let no_tools = match (args.no_tools, args.no_builtin_tools) {
            (true, _) => Some(NoTools::All),
            (false, true) => Some(NoTools::Builtin),
            (false, false) => None,
        };
        let append = &args.append_system_prompt;
        let resources = ResourceOptions {
            skill_paths: resolve_cli_paths(cwd, &args.skills),
            prompt_template_paths: resolve_cli_paths(cwd, &args.prompt_templates),
            theme_paths: resolve_cli_paths(cwd, &args.themes),
            extension_sources: resolve_cli_paths(cwd, &args.extensions),
            no_extensions: args.no_extensions,
            no_skills: args.no_skills,
            no_prompt_templates: args.no_prompt_templates,
            no_themes: args.no_themes,
            no_context_files: args.no_context_files,
            system_prompt: args.system_prompt.as_deref().map(|input| resolve_prompt_arg(input, cwd)),
            append_system_prompt: (!append.is_empty())
                .then(|| append.iter().map(|input| resolve_prompt_arg(input, cwd)).collect()),
            disabled_builtin_extensions: if args.no_mcp { vec!["mcp".into()] } else { Vec::new() },
        };
        Self {
            agent_dir: agent_dir.to_path_buf(),
            provider: args.provider.clone(),
            model: args.model.clone(),
            api_key: args.api_key.clone(),
            thinking: args.thinking,
            models: args.models.clone(),
            tools: args.tools.clone(),
            exclude_tools: args.exclude_tools.clone(),
            no_tools,
            resources,
            project_trust_override: args.project_trust_override,
            ..Self::default()
        }
    }
}

/// Trust decisions per cwd, made once per process (upstream `projectTrustByCwd`).
struct TrustCache {
    store: ProjectTrustStore,
    decided: Mutex<HashMap<PathBuf, bool>>,
}

impl TrustCache {
    fn project_trusted(&self, options: &AppOptions, cwd: &Path, agent_dir: &Path) -> (bool, Option<RuntimeDiagnostic>) {
        if let Some(settings) = &options.settings {
            return (settings.is_project_trusted(), None);
        }
        if let Some(trusted) = lock(&self.decided).get(cwd) {
            return (*trusted, None);
        }
        let global = SettingsManager::create(cwd, agent_dir, SettingsOptions { project_trusted: false });
        let home = home_dir();
        let request = ProjectTrustRequest {
            cwd,
            home: &home,
            trust_override: options.project_trust_override,
            default_project_trust: DefaultProjectTrust::parse(global.default_project_trust()),
            extension_decision: None,
            has_ui: false,
        };
        let (trusted, diagnostic) = match resolve_project_trust(&self.store, &request) {
            Ok(ProjectTrustResolution::Decided(trusted)) => (trusted, None),
            Ok(ProjectTrustResolution::Prompt(_)) => (false, None),
            Err(error) => (false, Some(RuntimeDiagnostic::warning(format!("Could not read project trust: {error:#}")))),
        };
        lock(&self.decided).insert(cwd.to_path_buf(), trusted);
        (trusted, diagnostic)
    }
}

async fn create_runtime_parts(
    options: &AppOptions,
    trust: &TrustCache,
    target: RuntimeTarget,
) -> anyhow::Result<RuntimeParts> {
    let (project_trusted, trust_diagnostic) = trust.project_trusted(options, &target.cwd, &target.agent_dir);
    let services = create_agent_session_services(CreateServicesOptions {
        cwd: target.cwd.clone(),
        agent_dir: target.agent_dir.clone(),
        settings: options.settings.clone(),
        project_trusted: Some(project_trusted),
        model_runtime: None,
        credentials: options.credentials.clone(),
        oauth: options.oauth.clone(),
        resources: options.resources.clone(),
    })
    .await;
    let mut diagnostics: Vec<RuntimeDiagnostic> = trust_diagnostic.into_iter().collect();
    diagnostics.extend(services.diagnostics.iter().cloned());
    let inputs = setup::CliInputs {
        provider: options.provider.as_deref(),
        model: options.model.as_deref(),
        thinking: options.thinking,
        models: options.models.as_deref(),
        has_existing_session: !target.session_manager.build_session_context().messages.is_empty(),
    };
    let choice = setup::choose_cli_model(&inputs, &services.settings, services.model_runtime.as_ref());
    diagnostics.extend(choice.warnings.iter().map(RuntimeDiagnostic::warning));
    diagnostics.extend(choice.errors.iter().map(RuntimeDiagnostic::error));
    if let Some(key) = &options.api_key {
        match &choice.model {
            Some(model) => services.model_runtime.set_runtime_api_key(&model.provider, key),
            None => diagnostics.push(RuntimeDiagnostic::error(
                "--api-key requires a model to be specified via --model, --provider/--model, or --models",
            )),
        }
    }
    let mut create = CreateAgentSessionOptions::new(services, target.session_manager);
    create.model = choice.model;
    create.thinking_level = choice.thinking;
    create.scoped_models = choice.scoped;
    create.tools = options.tools.clone();
    create.exclude_tools = options.exclude_tools.clone();
    create.no_tools = options.no_tools;
    create.custom_tools = options.custom_tools.clone();
    create.stream_fn = options.stream_fn.clone();
    create.mcp = options.mcp.clone();
    let created = create_agent_session(create).await?;
    Ok(RuntimeParts { session: created.session, model_fallback_message: created.model_fallback_message, diagnostics })
}

/// The factory that applies `options` to every session the runtime creates.
pub fn runtime_factory(options: AppOptions) -> RuntimeFactory {
    let trust =
        Arc::new(TrustCache { store: ProjectTrustStore::new(&options.agent_dir), decided: Mutex::new(HashMap::new()) });
    let options = Arc::new(options);
    Arc::new(move |target: RuntimeTarget| {
        let (options, trust) = (options.clone(), trust.clone());
        async move { create_runtime_parts(&options, &trust, target).await }.boxed()
    })
}

/// Create the runtime for `session_manager` with the command-line options.
pub async fn create_app_runtime(
    options: AppOptions,
    session_manager: SessionManager,
) -> anyhow::Result<AgentSessionRuntime> {
    let cwd = session_manager.cwd().to_path_buf();
    let agent_dir = options.agent_dir.clone();
    AgentSessionRuntime::create(runtime_factory(options), &cwd, &agent_dir, session_manager).await
}
