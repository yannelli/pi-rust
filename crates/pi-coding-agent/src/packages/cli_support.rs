//! Shared pieces of `pi install/remove/update/list/config`: callbacks owned by other
//! streams, project trust for commands, settings warnings, and output styling.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use crate::resources::trust::{
    DefaultProjectTrust, ExtensionTrustDecision, ProjectTrustOption, ProjectTrustPrompt, ProjectTrustRequest,
    ProjectTrustResolution, ProjectTrustStore, apply_project_trust_choice, has_trust_requiring_project_resources,
    resolve_project_trust,
};
use crate::settings::{SettingsManager, SettingsOptions};

/// `pi update --models`: refresh model catalogs (owned by the model catalog stream).
pub type ModelsUpdateFn = Arc<dyn Fn(PathBuf) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// What `pi update --self` (or `pi update pi`) asks the self-update owner to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfUpdateRequest {
    pub force: bool,
    pub agent_dir: PathBuf,
    /// The installation directory (`PI_PACKAGE_DIR` or the executable's directory).
    pub package_dir: PathBuf,
    /// The global `npmCommand` setting, used by npm-managed installs.
    pub npm_command: Option<Vec<String>>,
}

/// Self-update; prints its own progress and returns the exit code.
pub type SelfUpdateFn = Arc<dyn Fn(SelfUpdateRequest) -> BoxFuture<'static, i32> + Send + Sync>;

/// Shows the project trust prompt; `None` means cancelled (untrusted).
pub type TrustPromptFn =
    Arc<dyn Fn(ProjectTrustPrompt) -> BoxFuture<'static, Option<ProjectTrustOption>> + Send + Sync>;

/// Asks loaded extensions for a `project_trust` decision before the saved one is used.
pub type TrustExtensionsFn =
    Arc<dyn Fn(PathBuf) -> BoxFuture<'static, (Option<ExtensionTrustDecision>, Vec<String>)> + Send + Sync>;

/// ANSI styling like upstream's chalk calls, off unless `enabled`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Style {
    pub enabled: bool,
}

impl Style {
    fn wrap(self, code: &str, text: &str) -> String {
        if self.enabled { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
    }
    pub fn red(self, text: &str) -> String {
        self.wrap("31", text)
    }
    pub fn green(self, text: &str) -> String {
        self.wrap("32", text)
    }
    pub fn yellow(self, text: &str) -> String {
        self.wrap("33", text)
    }
    pub fn dim(self, text: &str) -> String {
        self.wrap("2", text)
    }
    pub fn bold(self, text: &str) -> String {
        self.wrap("1", text)
    }
}

/// A shared output stream of a command: process stdout or stderr, or a buffer in tests.
#[derive(Clone)]
pub struct CommandOutput(Arc<Mutex<Box<dyn Write + Send>>>);

struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuffer {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CommandOutput {
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self(Arc::new(Mutex::new(writer)))
    }
    pub fn stdout() -> Self {
        Self::new(Box::new(std::io::stdout()))
    }
    pub fn stderr() -> Self {
        Self::new(Box::new(std::io::stderr()))
    }
    /// An output that collects text, and the buffer to read it from.
    pub fn capture() -> (Self, Arc<Mutex<Vec<u8>>>) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        (Self::new(Box::new(SharedBuffer(buffer.clone()))), buffer)
    }
    pub(crate) fn line(&self, text: &str) {
        let mut writer = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = writeln!(writer, "{text}");
        let _ = writer.flush();
    }
}

/// Output streams and styling of one command run.
pub(crate) struct Io {
    pub out: CommandOutput,
    pub err: CommandOutput,
    pub style: Style,
}

impl Io {
    pub fn line(&self, text: &str) {
        self.out.line(text);
    }
    pub fn error(&self, text: &str) {
        self.err.line(text);
    }
}

/// Inputs for resolving project trust of a command.
pub(crate) struct TrustInputs<'a> {
    pub cwd: &'a Path,
    pub agent_dir: &'a Path,
    pub home: &'a Path,
    pub trust_override: Option<bool>,
    /// `pi update` uses only the saved decision: no prompt, no extensions, no default.
    pub saved_only: bool,
    pub has_ui: bool,
    pub prompt: Option<&'a TrustPromptFn>,
    pub extensions: Option<&'a TrustExtensionsFn>,
}

/// Settings for a package command with project trust resolved (upstream
/// `createCommandSettingsManager`); also returns trust warnings to print.
pub(crate) async fn command_settings(inputs: TrustInputs<'_>) -> (Arc<SettingsManager>, Vec<String>) {
    let settings = SettingsManager::create(inputs.cwd, inputs.agent_dir, SettingsOptions { project_trusted: false });
    let store = ProjectTrustStore::new(inputs.agent_dir);
    let mut warnings = Vec::new();
    if inputs.saved_only {
        let saved = store.get(inputs.cwd).ok().flatten() == Some(true);
        settings.set_project_trusted(inputs.trust_override.unwrap_or(saved));
        return (Arc::new(settings), warnings);
    }
    let mut extension_decision = None;
    if inputs.trust_override.is_none()
        && has_trust_requiring_project_resources(inputs.cwd, inputs.home)
        && let Some(extensions) = inputs.extensions
    {
        let (decision, errors) = extensions(inputs.cwd.to_path_buf()).await;
        extension_decision = decision;
        warnings.extend(errors);
    }
    let request = ProjectTrustRequest {
        cwd: inputs.cwd,
        home: inputs.home,
        trust_override: inputs.trust_override,
        default_project_trust: DefaultProjectTrust::parse(settings.default_project_trust()),
        extension_decision,
        has_ui: inputs.has_ui,
    };
    let trusted = match resolve_project_trust(&store, &request) {
        Ok(ProjectTrustResolution::Decided(trusted)) => trusted,
        Ok(ProjectTrustResolution::Prompt(prompt)) => match inputs.prompt {
            Some(prompt_fn) => {
                let choice = prompt_fn(prompt).await;
                apply_project_trust_choice(&store, choice.as_ref()).unwrap_or_else(|error| {
                    warnings.push(error.to_string());
                    false
                })
            }
            None => false,
        },
        Err(error) => {
            warnings.push(error.to_string());
            false
        }
    };
    settings.set_project_trusted(trusted);
    (Arc::new(settings), warnings)
}

/// `Warning (<context>, <scope> settings): <message>` for each settings problem.
pub(crate) fn report_settings_errors(settings: &SettingsManager, context: &str, io: &Io) {
    for error in settings.drain_errors() {
        let text = format!("Warning ({context}, {} settings): {}", error.scope.as_str(), error.message);
        let styled = io.style.yellow(&text);
        io.error(&styled);
    }
}

/// A numbered trust prompt on the terminal, for callers without a TUI selector.
pub fn prompt_trust_on_terminal(prompt: &ProjectTrustPrompt) -> Option<ProjectTrustOption> {
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "{}\n", prompt.message);
    for (index, option) in prompt.options.iter().enumerate() {
        let _ = writeln!(stderr, "  {}. {}", index + 1, option.label);
    }
    let _ = write!(stderr, "Choose [1-{}]: ", prompt.options.len());
    let _ = stderr.flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).ok()?;
    let choice = answer.trim().parse::<usize>().ok()?;
    prompt.options.get(choice.checked_sub(1)?).cloned()
}
