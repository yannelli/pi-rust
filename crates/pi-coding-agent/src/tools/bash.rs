//! `bash` tool: the shell tool with bash (upstream `createBashTool`).

use std::path::PathBuf;
use std::sync::Arc;

use super::shell::{SessionEnvironmentFn, ShellSpawnHook, ShellTool, ShellToolConfig};
use super::shell_process::{LocalShellOperations, ShellOperations};

pub const BASH_TOOL_CONFIG: ShellToolConfig = ShellToolConfig {
    name: "bash",
    label: "bash",
    shell_name: "bash",
    prompt: "$",
    prompt_snippet: "Execute bash commands (ls, grep, find, etc.)",
    prompt_guidelines: &["You can inspect PI_* environment variables for current model and session details."],
    temp_file_prefix: "pi-bash",
};

/// The `bash` tool. Built by [`ShellTool::new`] or [`ShellTool::bash`].
pub type BashTool = ShellTool;

/// Options of the bash tool (upstream `BashToolOptions`).
#[derive(Clone)]
pub struct BashToolOptions {
    /// Runs the commands. Default: local processes with `shell_path`.
    pub operations: Option<Arc<dyn ShellOperations>>,
    /// A line run before every command, such as `shopt -s expand_aliases`.
    pub command_prefix: Option<String>,
    /// The `shellPath` setting: run commands with this shell; a missing file fails each command.
    pub shell_path: Option<PathBuf>,
    /// Expose the session's `PI_*` variables to commands. Default: true.
    pub expose_session_environment: bool,
    pub spawn_hook: Option<ShellSpawnHook>,
    /// Supplies the session variables. Sessions supply their own; standalone tools default to
    /// [`super::set_session_environment`].
    pub session_environment: Option<SessionEnvironmentFn>,
}

impl Default for BashToolOptions {
    fn default() -> Self {
        Self {
            operations: None,
            command_prefix: None,
            shell_path: None,
            expose_session_environment: true,
            spawn_hook: None,
            session_environment: None,
        }
    }
}

impl ShellTool {
    /// The bash tool with default options.
    pub fn new(cwd: PathBuf) -> Self {
        Self::bash(cwd, BashToolOptions::default())
    }

    pub fn bash(cwd: PathBuf, options: BashToolOptions) -> Self {
        let operations =
            options.operations.unwrap_or_else(|| Arc::new(LocalShellOperations::bash(options.shell_path.clone())));
        let mut tool = Self::with_config(cwd, BASH_TOOL_CONFIG, operations);
        tool.command_prefix = options.command_prefix.filter(|prefix| !prefix.is_empty());
        tool.expose_session_environment = options.expose_session_environment;
        tool.spawn_hook = options.spawn_hook;
        tool.session_environment = options.session_environment;
        tool
    }

    /// Run commands with local bash at `shell` (`-c <command>`), replacing the operations.
    pub fn with_shell_path(mut self, shell: Option<PathBuf>) -> Self {
        self.operations = Arc::new(LocalShellOperations::bash(shell));
        self
    }

    /// Run `prefix` on its own line before every command, such as `shopt -s expand_aliases`.
    pub fn with_command_prefix(mut self, prefix: Option<String>) -> Self {
        self.command_prefix = prefix.filter(|prefix| !prefix.is_empty());
        self
    }
}
