//! `/terminal-setup` (pi-rust extension, after Claude Code): in iTerm2, turns on the iTerm2
//! integration (`terminal.iterm2`) and lets programs write the clipboard, which `/copy` needs.

use std::process::Stdio;
use std::time::Duration;

use pi_tui::env::ProcessEnv;

use super::iterm2::is_iterm2;
use super::mode::InteractiveMode;

const DEFAULTS_TIMEOUT: Duration = Duration::from_secs(5);

/// `defaults write com.googlecode.iterm2 AllowClipboardAccess -bool true`, on macOS only.
async fn allow_clipboard_access() -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("run /terminal-setup on the Mac that runs iTerm2".into());
    }
    let mut command = tokio::process::Command::new("defaults");
    command.args(["write", "com.googlecode.iterm2", "AllowClipboardAccess", "-bool", "true"]);
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
    let output = tokio::time::timeout(DEFAULTS_TIMEOUT, command.output())
        .await
        .map_err(|_| "defaults did not finish".to_string())?
        .map_err(|error| format!("could not run defaults: {error}"))?;
    if output.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&output.stderr).trim().to_string()) }
}

/// What `/terminal-setup` changed, and the steps that it cannot do.
pub fn setup_report(enabled: bool, clipboard: &Result<(), String>) -> String {
    let integration = if enabled {
        "- The iTerm2 integration is on (`terminal.iterm2` in `~/.pi/agent/settings.json`): the tab color follows the agent, a badge names the session, and a finished run requests attention."
    } else {
        "- `terminal.iterm2` is already set in `~/.pi/agent/settings.json`, so it did not change."
    };
    let clipboard = match clipboard {
        Ok(()) => {
            "- iTerm2 now lets programs write the clipboard, so `/copy` works. Restart iTerm2 to apply it.".to_string()
        }
        Err(error) => format!("- Clipboard access did not change: {error}."),
    };
    [
        integration,
        &clipboard,
        "",
        "Do these steps in iTerm2 if you want them:",
        "",
        "- To use Option as Alt in key bindings: Settings > Profiles > Keys > General. Set Left Option key and Right Option key to Esc+.",
        "- To see a notification when a run finishes: Settings > Profiles > Terminal. Turn on Notification Center Alerts, click Filter Alerts, and turn on Send escape sequence-generated alerts.",
        "- To show the agent state in the status bar: Settings > Profiles > Session > Configure Status Bar. Add an Interpolated String with `\\(user.pi_state)`. pi also sets `user.pi_session`, `user.pi_project`, and `user.pi_model`.",
        "- In tmux, run `tmux set -g allow-passthrough on` so that the tab color and the badge reach iTerm2.",
    ]
    .join("\n")
}

impl InteractiveMode {
    pub(super) fn handle_terminal_setup_command(&self) {
        if !is_iterm2(&ProcessEnv) {
            self.show_status("/terminal-setup sets up iTerm2, and this terminal is not iTerm2. Shift+Enter needs no setup in iTerm2, Ghostty, kitty, WezTerm, and Windows Terminal.");
            return;
        }
        let enabled = self.settings().enable_iterm2();
        self.iterm2_report_session();
        self.spawn(move |mode| async move {
            let clipboard = tokio::spawn(allow_clipboard_access()).await.unwrap_or_else(|error| Err(error.to_string()));
            mode.add_bordered_markdown("Terminal setup", &setup_report(enabled, &clipboard));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_says_what_changed() {
        let report = setup_report(true, &Ok(()));
        assert!(report.contains("The iTerm2 integration is on"));
        assert!(report.contains("Restart iTerm2"));
        assert!(report.contains("\\(user.pi_state)"));
        let report = setup_report(false, &Err("no defaults".into()));
        assert!(report.contains("already set"));
        assert!(report.contains("Clipboard access did not change: no defaults."));
    }
}
