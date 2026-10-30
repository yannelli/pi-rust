//! `pi doctor` (a pi-rust extension): a short report on the install, `PATH`, config files,
//! credentials, MCP servers, and terminal. It only reads files and the environment: no network,
//! no MCP server starts, and no terminal queries. It exits 1 when it finds a problem.

mod checks;
mod render;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use pi_ai::auth::{AuthType, CredentialStore, FileCredentialStore, OAuthLookup};
use serde::Serialize;

pub use checks::{find_command, find_on_path, target_triple};
pub use render::render_text;

use crate::cli::CommandOutput;
use crate::config::{auth_path, models_path};
use crate::packages::Marketplace;
use crate::packages::moved_features::doctor_warnings;
use crate::resources::brand::APP_NAME;
use crate::settings::{SettingsManager, SettingsOptions, SettingsScope};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    pub ok: bool,
    pub version: String,
    pub target: String,
    pub install: InstallReport,
    pub path: PathReport,
    pub config: ConfigReport,
    pub auth: AuthReport,
    pub mcp: McpReport,
    pub terminal: TerminalReport,
    pub problems: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallReport {
    /// `release` (`pi update --self` replaces it), `cargo`, or `unknown`.
    pub method: String,
    pub executable: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PathReport {
    /// Every `pi` on `PATH`, in lookup order.
    pub found: Vec<String>,
    /// Index of this executable in `found`.
    pub current_index: Option<usize>,
    /// This executable is one of them.
    pub on_path: bool,
    /// This executable is on `PATH`, but another `pi` comes first.
    pub shadowed: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFile {
    pub name: String,
    pub path: String,
    pub exists: bool,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReport {
    pub agent_dir: String,
    pub agent_dir_exists: bool,
    pub files: Vec<ConfigFile>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredential {
    pub provider: String,
    pub auth_type: AuthType,
    /// Where the credential comes from, such as `stored credential` or an environment variable name.
    pub source: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthReport {
    pub providers: Vec<ProviderCredential>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerReport {
    pub name: String,
    pub scope: Option<pi_mcp::McpConfigScope>,
    pub enabled: bool,
    /// `stdio` or `http`.
    pub transport: String,
    /// The stdio command, without arguments.
    pub command: Option<String>,
    /// Where the stdio command was found.
    pub resolved: Option<String>,
    /// Scheme, host, and port of an HTTP server; the rest of the URL can hold tokens.
    pub origin: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpReport {
    pub project_trusted: bool,
    pub servers: Vec<McpServerReport>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalReport {
    pub term: Option<String>,
    pub term_program: Option<String>,
    pub colorterm: Option<String>,
    /// Both stdin and stdout are terminals.
    pub interactive: bool,
    /// `truecolor` or `256`.
    pub color_depth: String,
    /// `kitty`, `iterm2`, or `None` when images are off.
    pub image_protocol: Option<String>,
    pub hyperlinks: bool,
    /// Always `None`: pi negotiates the keyboard protocol with a terminal query at startup.
    pub keyboard_protocol: Option<String>,
}

/// What `pi doctor` reads; tests replace the executable and `PATH`.
#[derive(Clone)]
pub struct DoctorInputs {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub executable: Option<PathBuf>,
    pub path_var: Option<OsString>,
    pub home: PathBuf,
    pub cargo_home: Option<PathBuf>,
    pub oauth: OAuthLookup,
}

impl DoctorInputs {
    pub fn from_env(cwd: PathBuf, agent_dir: PathBuf, oauth: OAuthLookup) -> Self {
        Self {
            cwd,
            agent_dir,
            executable: std::env::current_exe().ok(),
            path_var: std::env::var_os("PATH"),
            home: crate::config::home_dir(),
            cargo_home: std::env::var_os("CARGO_HOME").filter(|home| !home.is_empty()).map(PathBuf::from),
            oauth,
        }
    }
}

fn config_file(name: &str, path: PathBuf, errors: Vec<String>) -> ConfigFile {
    ConfigFile { name: name.into(), exists: path.exists(), path: path.display().to_string(), errors }
}

/// One line per error; `models.json` errors span several lines.
fn one_line(text: &str) -> String {
    text.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(" ")
}

async fn config_and_auth(inputs: &DoctorInputs, problems: &mut Vec<String>) -> (ConfigReport, AuthReport) {
    let settings = SettingsManager::create(&inputs.cwd, &inputs.agent_dir, SettingsOptions { project_trusted: true });
    let settings_errors = settings.drain_errors();
    problems.extend(settings_errors.iter().map(|error| error.diagnostic()));
    let scope_errors = |scope: SettingsScope| {
        settings_errors.iter().filter(|error| error.scope == scope).map(|error| error.message.clone()).collect()
    };
    let store = Arc::new(FileCredentialStore::read_only(auth_path(&inputs.agent_dir)));
    let auth_errors: Vec<String> =
        store.list().await.err().map(|error| one_line(&error.to_string())).into_iter().collect();
    problems.extend(auth_errors.iter().map(|error| format!("auth.json: {error}")));
    let context = crate::auth_cli::auth_context(&inputs.agent_dir, store, inputs.oauth.clone()).await;
    let models_errors: Vec<String> = context.models_error.iter().map(|error| one_line(error)).collect();
    problems.extend(models_errors.iter().cloned());
    let mut providers = Vec::new();
    for provider in context.registry.providers() {
        if let Ok(Some(check)) = context.resolver.check(&provider).await {
            providers.push(ProviderCredential { provider, auth_type: check.auth_type, source: check.source });
        }
    }
    providers.sort_by(|a, b| a.provider.cmp(&b.provider));
    let project_settings = crate::resources::brand::project_config_dir(&inputs.cwd).join("settings.json");
    let files = vec![
        config_file("settings", crate::config::settings_path(&inputs.agent_dir), scope_errors(SettingsScope::Global)),
        config_file("project settings", project_settings, scope_errors(SettingsScope::Project)),
        config_file("models", models_path(&inputs.agent_dir), models_errors),
        config_file("auth", auth_path(&inputs.agent_dir), auth_errors),
    ];
    let config = ConfigReport {
        agent_dir: inputs.agent_dir.display().to_string(),
        agent_dir_exists: inputs.agent_dir.is_dir(),
        files,
    };
    (config, AuthReport { providers })
}

/// Legacy `statusLine` and `terminal.iterm2` settings whose marketplace extension is missing.
fn moved_feature_warnings(inputs: &DoctorInputs) -> Vec<String> {
    let settings = SettingsManager::create(&inputs.cwd, &inputs.agent_dir, SettingsOptions { project_trusted: true });
    let marketplace = Marketplace::new(inputs.agent_dir.clone(), reqwest::Client::new(), true);
    doctor_warnings(&settings, &marketplace)
}

/// Build the whole report.
pub async fn collect(inputs: &DoctorInputs) -> DoctorReport {
    let mut problems = Vec::new();
    let mut warnings = Vec::new();
    let install = checks::install(inputs);
    let path = checks::path(inputs);
    let current = install.executable.clone().unwrap_or_else(|| "this program".into());
    if let Some(first) = path.found.first().filter(|_| path.shadowed) {
        problems.push(format!("Another {APP_NAME} comes first on PATH: {first} runs instead of {current}"));
    } else if let Some(first) = path.found.first().filter(|_| !path.on_path) {
        warnings.push(format!("{first} is the {APP_NAME} on PATH; {current} is not on PATH"));
    } else if path.found.is_empty() {
        warnings.push(format!("No {APP_NAME} on PATH"));
    }
    let (config, auth) = config_and_auth(inputs, &mut problems).await;
    if auth.providers.is_empty() {
        warnings
            .push(format!("No provider has credentials. Run /login or `{APP_NAME} auth`, or set an API key variable."));
    }
    warnings.extend(crate::migrations::deprecation_warnings(&inputs.cwd, &inputs.agent_dir));
    warnings.extend(moved_feature_warnings(inputs));
    let mcp = checks::mcp(inputs, &mut problems);
    DoctorReport {
        ok: problems.is_empty(),
        version: crate::platform::VERSION.to_string(),
        target: target_triple(),
        install,
        path,
        config,
        auth,
        mcp,
        terminal: checks::terminal(),
        problems,
        warnings,
    }
}

pub fn doctor_help() -> String {
    format!(
        "Usage:
  {APP_NAME} doctor [--json]

Check this {APP_NAME}: version and install, other {APP_NAME} programs on PATH, settings, models.json,
auth.json, credentials (names and sources only), MCP server commands, and terminal support.
It reads files only: no network calls, no MCP servers start, and the terminal is not queried.
Exits 1 when it finds a problem.

Options:
  --json   Print the report as JSON
"
    )
}

/// Run `pi doctor ...`; `None` when `raw` is another command.
pub async fn run_doctor_command(raw: &[String], inputs: impl FnOnce() -> DoctorInputs) -> Option<CommandOutput> {
    if raw.first().map(String::as_str) != Some("doctor") {
        return None;
    }
    let mut json = false;
    for arg in &raw[1..] {
        match arg.as_str() {
            "-h" | "--help" | "help" => return Some(CommandOutput { stdout: doctor_help(), ..Default::default() }),
            "--json" => json = true,
            other => {
                let stderr = format!("Error: Unknown argument {other}.\nUsage: {APP_NAME} doctor [--json]\n");
                return Some(CommandOutput { code: 1, stdout: String::new(), stderr });
            }
        }
    }
    let report = collect(&inputs()).await;
    let stdout = if json {
        format!("{}\n", serde_json::to_string_pretty(&report).unwrap_or_default())
    } else {
        render_text(&report)
    };
    Some(CommandOutput { code: if report.ok { 0 } else { 1 }, stdout, stderr: String::new() })
}
