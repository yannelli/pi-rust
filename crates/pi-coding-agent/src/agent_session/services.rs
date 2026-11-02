//! Services bound to one working directory (upstream `agent-session-services.ts`): settings,
//! the model runtime, and the resource loader. They are recreated when the session cwd changes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use pi_ai::auth::{CredentialStore, OAuthLookup};
use pi_ai::env::EnvLookup;

use super::model_runtime::{ModelRuntime, ModelRuntimeOptions};
use super::runtime_catalog::CatalogSetup;
use crate::packages::{PackageManager, PackageManagerOptions};
use crate::resources::{ResolvedPaths, ResourceDiagnostic, ResourceLoader, ResourceLoaderOptions, ResourceSettings};
use crate::settings::{ResourceKind, SettingsManager, SettingsOptions, SettingsScope};

/// How bad a runtime diagnostic is; the app decides what to print and whether to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

/// A non-fatal issue collected while creating services or sessions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeDiagnostic {
    pub level: DiagnosticLevel,
    pub message: String,
}

impl RuntimeDiagnostic {
    pub fn warning(message: impl Into<String>) -> Self {
        Self { level: DiagnosticLevel::Warning, message: message.into() }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self { level: DiagnosticLevel::Error, message: message.into() }
    }
}

/// CLI resource options, with local paths already resolved against the startup cwd.
#[derive(Clone, Debug, Default)]
pub struct ResourceOptions {
    pub skill_paths: Vec<String>,
    pub prompt_template_paths: Vec<String>,
    pub theme_paths: Vec<String>,
    pub extension_sources: Vec<String>,
    pub no_extensions: bool,
    pub no_skills: bool,
    pub no_prompt_templates: bool,
    pub no_themes: bool,
    pub no_context_files: bool,
    /// `--system-prompt`: replaces `SYSTEM.md`.
    pub system_prompt: Option<String>,
    /// `--append-system-prompt` values: replace `APPEND_SYSTEM.md`.
    pub append_system_prompt: Option<Vec<String>>,
    /// `--output-schema` instructions, appended after the append system prompt texts (pi-rust).
    pub output_schema_prompt: Option<String>,
    /// Built-in extensions turned off for this run, such as `mcp` for `--no-mcp`.
    pub disabled_builtin_extensions: Vec<String>,
    /// Home directory for user-level resources such as `~/.agents/skills`; `None` uses the real one.
    pub home: Option<PathBuf>,
}

/// Inputs of [`create_agent_session_services`]. Defaults load everything from `agent_dir`.
#[derive(Clone, Default)]
pub struct CreateServicesOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub settings: Option<Arc<SettingsManager>>,
    /// Whether project settings and resources load, when `settings` is not given.
    pub project_trusted: Option<bool>,
    pub model_runtime: Option<Arc<ModelRuntime>>,
    /// Credential store for a new model runtime, in place of `auth.json`.
    pub credentials: Option<Arc<dyn CredentialStore>>,
    /// OAuth providers for a new model runtime; `None` uses the built-in ones.
    pub oauth: Option<OAuthLookup>,
    /// Environment a new model runtime reads credentials from; `None` is the process environment.
    pub credential_env: Option<Arc<dyn EnvLookup>>,
    pub resources: ResourceOptions,
}

impl CreateServicesOptions {
    pub fn new(cwd: &Path, agent_dir: &Path) -> Self {
        Self { cwd: cwd.to_path_buf(), agent_dir: agent_dir.to_path_buf(), ..Self::default() }
    }
}

/// Settings, models, and resources for one effective session cwd.
#[derive(Clone)]
pub struct AgentSessionServices {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub model_runtime: Arc<ModelRuntime>,
    pub settings: Arc<SettingsManager>,
    pub resource_loader: Arc<Mutex<ResourceLoader>>,
    pub diagnostics: Vec<RuntimeDiagnostic>,
}

impl AgentSessionServices {
    /// The resource loader, locked.
    pub fn resources(&self) -> MutexGuard<'_, ResourceLoader> {
        self.resource_loader.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn resource_settings(settings: &SettingsManager, scope: SettingsScope) -> ResourceSettings {
    ResourceSettings {
        extensions: settings.resource_paths(ResourceKind::Extensions, scope),
        skills: settings.resource_paths(ResourceKind::Skills, scope),
        prompts: settings.resource_paths(ResourceKind::Prompts, scope),
        themes: settings.resource_paths(ResourceKind::Themes, scope),
    }
}

/// Resource loader options for `cwd` from settings and CLI options.
pub fn resource_loader_options(
    cwd: &Path,
    agent_dir: &Path,
    settings: &SettingsManager,
    resources: &ResourceOptions,
) -> ResourceLoaderOptions {
    let mut options = ResourceLoaderOptions::new(cwd, agent_dir);
    options.project_trusted = settings.is_project_trusted();
    options.user_settings = resource_settings(settings, SettingsScope::Global);
    options.project_settings = resource_settings(settings, SettingsScope::Project);
    options.skill_paths = resources.skill_paths.clone();
    options.prompt_template_paths = resources.prompt_template_paths.clone();
    options.theme_paths = resources.theme_paths.clone();
    options.extension_sources = resources.extension_sources.clone();
    options.no_extensions = resources.no_extensions;
    options.no_skills = resources.no_skills;
    options.no_prompt_templates = resources.no_prompt_templates;
    options.no_themes = resources.no_themes;
    options.no_context_files = resources.no_context_files;
    options.system_prompt = resources.system_prompt.clone();
    options.append_system_prompt = resources.append_system_prompt.clone();
    options.output_schema_prompt = resources.output_schema_prompt.clone();
    options.builtin_extensions = super::builtins::BUILTIN_EXTENSIONS.iter().map(|name| name.to_string()).collect();
    options.disabled_builtin_extensions = resources.disabled_builtin_extensions.clone();
    if let Some(home) = &resources.home {
        options.home = home.clone();
    }
    options
}

/// Whether the built-in extension `name` loads (`builtin:<name>` among the extensions).
pub fn builtin_enabled(loader: &ResourceLoader, name: &str) -> bool {
    let path = format!("{}{name}", crate::resources::source_info::BUILTIN_PATH_PREFIX);
    loader.resources().extensions.iter().any(|entry| entry.path.to_string_lossy() == path)
}

/// Resource problems as diagnostics: errors stay errors, everything else is a warning.
pub fn resource_diagnostics(loader: &ResourceLoader) -> Vec<RuntimeDiagnostic> {
    let loaded = loader.resources();
    let lists: [&[ResourceDiagnostic]; 4] = [
        &loaded.extension_diagnostics,
        &loaded.skill_diagnostics,
        &loaded.prompt_diagnostics,
        &loaded.theme_diagnostics,
    ];
    lists
        .into_iter()
        .flatten()
        .map(|diagnostic| {
            let message = match &diagnostic.path {
                Some(path) if !diagnostic.message.contains(path.as_str()) => {
                    format!("{}: {path}", diagnostic.message)
                }
                _ => diagnostic.message.clone(),
            };
            match diagnostic.kind {
                crate::resources::DiagnosticKind::Error => RuntimeDiagnostic::error(message),
                _ => RuntimeDiagnostic::warning(message),
            }
        })
        .collect()
}

/// Resources of the `packages` settings; missing packages install first, as upstream. A
/// package that fails to install is left out with one warning, and the others still load.
pub async fn package_resources(
    cwd: &Path,
    agent_dir: &Path,
    settings: &Arc<SettingsManager>,
) -> (ResolvedPaths, Vec<RuntimeDiagnostic>) {
    let configured = [SettingsScope::Global, SettingsScope::Project];
    if configured.iter().all(|scope| settings.packages(*scope).is_empty()) {
        return (ResolvedPaths::default(), Vec::new());
    }
    let manager = PackageManager::new(PackageManagerOptions::new(cwd, agent_dir, settings.clone()));
    let resolved = manager.resolve_package_resources(None).await;
    let mut diagnostics: Vec<RuntimeDiagnostic> =
        manager.take_install_failures().iter().map(|failure| RuntimeDiagnostic::warning(failure.warning())).collect();
    match resolved {
        Ok(paths) => (paths, diagnostics),
        Err(error) => {
            diagnostics.push(RuntimeDiagnostic::warning(format!("Failed to load packages: {error:#}")));
            (ResolvedPaths::default(), diagnostics)
        }
    }
}

/// Create settings, the model runtime, and the resource loader for `options.cwd`.
pub async fn create_agent_session_services(options: CreateServicesOptions) -> AgentSessionServices {
    let cwd = crate::session::ids::resolve_path(&options.cwd);
    let agent_dir = crate::session::ids::resolve_path(&options.agent_dir);
    let settings = options.settings.unwrap_or_else(|| {
        let project_trusted = options.project_trusted.unwrap_or(true);
        Arc::new(SettingsManager::create(&cwd, &agent_dir, SettingsOptions { project_trusted }))
    });
    let mut diagnostics: Vec<RuntimeDiagnostic> =
        settings.drain_diagnostics().iter().map(|error| RuntimeDiagnostic::warning(error.diagnostic())).collect();
    let model_runtime = match options.model_runtime {
        Some(runtime) => {
            runtime.set_disabled_providers(settings.global_settings().disabled_providers.unwrap_or_default());
            runtime
        }
        None => {
            let mut runtime_options = ModelRuntimeOptions::for_agent_dir(&agent_dir);
            if let Some(credentials) = options.credentials {
                runtime_options.credentials = credentials;
            }
            if options.oauth.is_some() {
                runtime_options.oauth = options.oauth;
            }
            if let Some(env) = options.credential_env {
                runtime_options.credential_env = env;
            }
            let runtime = ModelRuntime::create(runtime_options).await;
            runtime.set_disabled_providers(settings.global_settings().disabled_providers.unwrap_or_default());
            runtime.enable_dynamic_catalogs(CatalogSetup::for_agent_dir(&agent_dir)).await;
            Arc::new(runtime)
        }
    };
    if let Some(error) = model_runtime.load_error() {
        diagnostics.push(RuntimeDiagnostic::warning(format!("errors loading models.json:\n{error}")));
    }
    let (packages, package_diagnostics) = package_resources(&cwd, &agent_dir, &settings).await;
    diagnostics.extend(package_diagnostics);
    let mut loader_options = resource_loader_options(&cwd, &agent_dir, &settings, &options.resources);
    loader_options.package_resources = packages;
    let mut loader = ResourceLoader::new(loader_options);
    loader.reload();
    diagnostics.extend(resource_diagnostics(&loader));
    model_runtime.set_llama_enabled(builtin_enabled(&loader, crate::llama::LLAMA_PROVIDER_ID)).await;
    crate::acp_client::register::sync(&model_runtime, &settings, &agent_dir);
    AgentSessionServices {
        cwd,
        agent_dir,
        model_runtime,
        settings,
        resource_loader: Arc::new(Mutex::new(loader)),
        diagnostics,
    }
}
