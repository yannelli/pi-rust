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
    RuntimeDiagnostic, RuntimeFactory, RuntimeParts, RuntimeTarget, TrustPromptFn, create_agent_session,
    create_agent_session_services, lock,
};
use crate::cli::Args;
use crate::config::home_dir;
use crate::resources::paths::{is_local_path, resolve_path};
use crate::resources::trust::{
    DefaultProjectTrust, ProjectTrustRequest, ProjectTrustResolution, ProjectTrustStore, apply_project_trust_choice,
    resolve_project_trust,
};
use crate::session::SessionManager;
use crate::settings::{SettingsManager, SettingsOptions};

#[path = "app_extensions.rs"]
mod extensions;
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
    /// Trust answers given before the runtime started, such as the interactive startup prompt.
    pub project_trust_decisions: Vec<(PathBuf, bool)>,
    /// Virtual models registered in every session's model runtime.
    pub virtual_models: Vec<crate::virtual_models::VirtualModelDefinition>,
    /// Long flags the CLI does not know, for flags extensions register (`--plan`).
    pub extension_flags: Vec<(String, crate::cli::FlagValue)>,
    /// Extensions supplied by the program, loaded before the extension files.
    pub inline_extensions: Vec<Arc<dyn crate::extensions::InlineExtension>>,
    /// `ctx.mode` of `project_trust` handlers; print mode when unset.
    pub extension_mode: Option<crate::extension_ui::ExtensionMode>,
    /// `ctx.ui` of `project_trust` handlers; without one they have no UI.
    pub project_trust_ui: Option<(Arc<dyn crate::extension_ui::ExtensionUi>, crate::extension_ui::ExtensionMode)>,
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
            home: None,
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
            extension_flags: args.unknown_flags.clone(),
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
    /// Trust for `cwd`: the cached decision, else extension `project_trust` handlers, the
    /// saved decision, and `defaultProjectTrust`.
    async fn project_trusted(
        &self,
        options: &AppOptions,
        cwd: &Path,
        agent_dir: &Path,
        trust_prompt: Option<&TrustPromptFn>,
        trust_ui: Option<&crate::agent_session::TrustUi>,
    ) -> (bool, Vec<RuntimeDiagnostic>) {
        if let Some(settings) = &options.settings {
            return (settings.is_project_trusted(), Vec::new());
        }
        if let Some(trusted) = lock(&self.decided).get(cwd) {
            return (*trusted, Vec::new());
        }
        let global = SettingsManager::create(cwd, agent_dir, SettingsOptions { project_trusted: false });
        let home = home_dir();
        let needs_decision = options.project_trust_override.is_none()
            && crate::resources::trust::has_trust_requiring_project_resources(cwd, &home);
        let (extension_decision, mut diagnostics) = match (needs_decision, trust_ui) {
            (false, _) => (None, Vec::new()),
            (true, None) => extensions::trust_decision(options, cwd, agent_dir).await,
            (true, Some(ui)) => {
                let options = AppOptions { project_trust_ui: Some(ui.clone()), ..options.clone() };
                extensions::trust_decision(&options, cwd, agent_dir).await
            }
        };
        let request = ProjectTrustRequest {
            cwd,
            home: &home,
            trust_override: options.project_trust_override,
            default_project_trust: DefaultProjectTrust::parse(global.default_project_trust()),
            extension_decision,
            has_ui: trust_prompt.is_some(),
        };
        let trusted = match (resolve_project_trust(&self.store, &request), trust_prompt) {
            (Ok(ProjectTrustResolution::Decided(trusted)), _) => trusted,
            (Ok(ProjectTrustResolution::Prompt(prompt)), Some(ask)) => {
                let choice = ask(prompt).await;
                apply_project_trust_choice(&self.store, choice.as_ref()).unwrap_or_else(|error| {
                    diagnostics.push(RuntimeDiagnostic::warning(format!("Could not save project trust: {error:#}")));
                    choice.is_some_and(|choice| choice.trusted)
                })
            }
            (Ok(ProjectTrustResolution::Prompt(_)), None) => false,
            (Err(error), _) => {
                diagnostics.push(RuntimeDiagnostic::warning(format!("Could not read project trust: {error:#}")));
                false
            }
        };
        lock(&self.decided).insert(cwd.to_path_buf(), trusted);
        (trusted, diagnostics)
    }
}

async fn create_runtime_parts(
    options: &AppOptions,
    trust: &TrustCache,
    target: RuntimeTarget,
) -> anyhow::Result<RuntimeParts> {
    let (project_trusted, trust_diagnostics) = trust
        .project_trusted(
            options,
            &target.cwd,
            &target.agent_dir,
            target.trust_prompt.as_ref(),
            target.trust_ui.as_ref(),
        )
        .await;
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
    let mut diagnostics: Vec<RuntimeDiagnostic> = trust_diagnostics;
    diagnostics.extend(services.diagnostics.iter().cloned());
    // Extensions load before the model is chosen, so `--model` can name a provider they register.
    let loaded = extensions::load(options, &services).await;
    diagnostics.extend(loaded.diagnostics);
    diagnostics.extend(extensions::apply_flags(loaded.runner.as_ref(), &options.extension_flags).await);
    for definition in &options.virtual_models {
        if let Err(error) = services.model_runtime.register_virtual_model(definition.clone()) {
            diagnostics.push(RuntimeDiagnostic::error(format!("{error:#}")));
        }
    }
    let inputs = setup::CliInputs {
        provider: options.provider.as_deref(),
        model: options.model.as_deref(),
        thinking: options.thinking,
        models: options.models.as_deref(),
        inline_api_key: options.api_key.is_some(),
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
    create.extensions = loaded.runner;
    let created = create_agent_session(create).await?;
    Ok(RuntimeParts { session: created.session, model_fallback_message: created.model_fallback_message, diagnostics })
}

/// Flags the extensions of `cwd` register, for `--help`.
pub async fn extension_help_flags(options: &AppOptions, cwd: &Path) -> Vec<crate::cli::HelpFlag> {
    let trust = TrustCache { store: ProjectTrustStore::new(&options.agent_dir), decided: Mutex::default() };
    let (project_trusted, _) = trust.project_trusted(options, cwd, &options.agent_dir, None, None).await;
    let services = create_agent_session_services(CreateServicesOptions {
        cwd: cwd.to_path_buf(),
        agent_dir: options.agent_dir.clone(),
        settings: options.settings.clone(),
        project_trusted: Some(project_trusted),
        model_runtime: None,
        credentials: options.credentials.clone(),
        oauth: options.oauth.clone(),
        resources: options.resources.clone(),
    })
    .await;
    let Some(runner) = extensions::load(options, &services).await.runner else { return Vec::new() };
    let flags = runner
        .registry()
        .flags
        .iter()
        .map(|flag| crate::cli::HelpFlag {
            name: flag.name.clone(),
            kind: flag.kind.clone(),
            description: flag.description.clone(),
            extension_path: flag.extension_path.clone(),
        })
        .collect();
    runner.shutdown();
    flags
}

/// The `project_trust` decision of the extensions that load without project trust, for a
/// startup prompt; `None` when no handler decided or `cwd` needs no decision.
pub async fn extension_trust_decision(
    options: &AppOptions,
    cwd: &Path,
) -> (Option<crate::resources::trust::ExtensionTrustDecision>, Vec<RuntimeDiagnostic>) {
    let needs_decision = options.project_trust_override.is_none()
        && crate::resources::trust::has_trust_requiring_project_resources(cwd, &home_dir());
    if !needs_decision {
        return (None, Vec::new());
    }
    extensions::trust_decision(options, cwd, &options.agent_dir).await
}

/// `PackageCommandOptions.trust_extensions`: `pi install` and `pi config` ask the extensions'
/// `project_trust` handlers before the saved decision.
pub fn package_trust_extensions(agent_dir: PathBuf) -> crate::packages::TrustExtensionsFn {
    let options = AppOptions { agent_dir, ..AppOptions::default() };
    Arc::new(move |cwd: PathBuf| {
        let options = options.clone();
        async move {
            let (decision, diagnostics) = extension_trust_decision(&options, &cwd).await;
            (decision, diagnostics.into_iter().map(|diagnostic| diagnostic.message).collect())
        }
        .boxed()
    })
}

/// The factory that applies `options` to every session the runtime creates.
pub fn runtime_factory(options: AppOptions) -> RuntimeFactory {
    let decided = options.project_trust_decisions.iter().cloned().collect();
    let trust =
        Arc::new(TrustCache { store: ProjectTrustStore::new(&options.agent_dir), decided: Mutex::new(decided) });
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
