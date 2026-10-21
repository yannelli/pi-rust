//! The interactive TUI (upstream `modes/interactive/interactive-mode.ts`). Tests drive
//! [`InteractiveMode`] with a virtual terminal through [`InteractiveOptions::terminal`].

mod auth;
mod autocomplete;
mod aws_mfa;
mod background;
pub mod bridge;
mod bug;
mod chat;
mod commands;
pub mod components;
mod dialogs;
mod events;
mod extension_bind;
mod extension_editor_slot;
mod extension_overlay;
pub mod extension_ui;
mod extension_view;
pub mod footer_data;
mod hotkeys;
mod info;
mod init;
mod input;
mod layout;
mod llama;
mod mcp;
mod mode;
mod model_refresh;
mod navigation;
mod notify;
pub mod prompt_history;
mod prompts;
pub mod renderers;
mod resources_view;
pub mod selector;
mod settings;
mod share;
mod shutdown;
mod startup;
pub mod startup_ui;
mod status;
mod tmux;
pub mod util;

use std::path::PathBuf;
use std::sync::Arc;

use pi_ai::ContentBlock;
use pi_tui::TerminalRef;

pub use autocomplete::BUILTIN_SLASH_COMMANDS;
pub use hotkeys::hotkeys_markdown;
pub use mode::InteractiveMode;
pub use shutdown::FatalErrorReported;

use crate::agent_session::{AgentSessionRuntime, RuntimeDiagnostic};
use crate::settings::schema::TuiMode;

/// What the interactive mode starts with (upstream `InteractiveModeOptions`).
#[derive(Clone, Default)]
pub struct InteractiveOptions {
    /// Sent once the UI is ready, with `initial_images` attached.
    pub initial_message: Option<String>,
    pub initial_images: Vec<ContentBlock>,
    /// Further messages, sent one after another.
    pub initial_messages: Vec<String>,
    /// `--verbose`: show the full startup header even with `quietStartup`.
    pub verbose: bool,
    /// `--tui-mode`; the `tuiMode` setting when `None`.
    pub tui_mode: Option<TuiMode>,
    /// `--use-theme` for this run, in place of the `theme` setting.
    pub initial_theme_setting: Option<String>,
    /// Warnings and errors collected before the UI existed.
    pub startup_diagnostics: Vec<RuntimeDiagnostic>,
    /// Why the restored model could not be used.
    pub model_fallback_message: Option<String>,
    /// "What's New" since the last run (`startup::startup_changelog_once`).
    pub changelog_markdown: Option<String>,
    /// The terminal to draw on; the process terminal when `None`.
    pub terminal: Option<TerminalRef>,
    /// The agent directory; `PI_CODING_AGENT_DIR` or `~/.pi/agent` when `None`.
    pub agent_dir: Option<PathBuf>,
    /// Skip the release and package update checks after startup, as tests do.
    pub skip_update_checks: bool,
    /// Upstream `autoTrustOnReloadCwd` ([`crate::startup::auto_trust_on_reload_cwd`]).
    pub auto_trust_on_reload_cwd: Option<PathBuf>,
}

/// Run the interactive mode for `runtime` until the user quits: installs the keybindings of
/// `keybindings.json` and runs the UI in a `LocalSet`.
pub async fn run_interactive(runtime: Arc<AgentSessionRuntime>, options: InteractiveOptions) -> anyhow::Result<()> {
    let agent_dir = options.agent_dir.clone().unwrap_or_else(crate::config::agent_dir);
    crate::keybindings::KeybindingsManager::create(&agent_dir).install();
    let options = InteractiveOptions { agent_dir: Some(agent_dir), ..options };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let mode = InteractiveMode::new(runtime, options);
            mode.run().await
        })
        .await
}
