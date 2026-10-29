//! `/terminal-setup` (pi-rust extension, after Claude Code): in iTerm2, turns on the iTerm2
//! integration (`terminal.iterm2`) and lets programs write the clipboard, which `/copy` needs.

use std::process::Stdio;
use std::time::Duration;

use pi_tui::env::ProcessEnv;

use super::iterm2::is_iterm2;
use super::mode::InteractiveMode;

const DEFAULTS_TIMEOUT: Duration = Duration::from_secs(5);

fn local_mac() -> bool {
    cfg!(target_os = "macos") && std::env::var_os("SSH_CONNECTION").is_none() && std::env::var_os("SSH_TTY").is_none()
}

async fn setup_command(program: &str, args: &[&str]) -> Result<String, String> {
    let mut command = tokio::process::Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let output = tokio::time::timeout(DEFAULTS_TIMEOUT, command.output())
        .await
        .map_err(|_| format!("{program} timed out; grant iTerm2 API access if prompted, then retry"))?
        .map_err(|error| format!("could not run {program}: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{program} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim().chars().take(1000).collect::<String>()
        ))
    }
}

/// Target this pane, never whichever pane currently has focus.
fn iterm_session_id(value: &str) -> Option<&str> {
    let id = value.rsplit(':').next()?;
    (id.starts_with(|c: char| c.is_ascii_alphanumeric()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .then_some(id)
}

const SESSION_PROPERTIES: [(&str, &str); 4] = [
    ("Option Key Sends", "2"),
    ("Right Option Key Sends", "2"),
    ("BM Growl", "true"),
    ("Send Terminal Generated Alerts", "true"),
];

async fn configure_it2_session(id: &str) -> Result<(), String> {
    for (property, value) in SESSION_PROPERTIES {
        setup_command("it2", &["profile", "session-set-property", id, property, value])
            .await
            .map_err(|error| format!("{property}: {error}. Earlier steps may have applied."))?;
    }
    Ok(())
}

/// `defaults write com.googlecode.iterm2 AllowClipboardAccess -bool true`, on macOS only.
async fn allow_clipboard_access() -> Result<(), String> {
    if !local_mac() {
        return Err("run /terminal-setup locally on the Mac that runs iTerm2, not over SSH".into());
    }
    setup_command("defaults", &["write", "com.googlecode.iterm2", "AllowClipboardAccess", "-bool", "true"])
        .await
        .map(|_| ())
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
        "For optional automation, install [tmc/it2](https://github.com/tmc/it2) on your Mac and run `it2 auth check`. Enable iTerm2 Settings > General > Magic > Enable Python API if required, and approve its access prompt. Run `/terminal-setup` again: Pi offers to set Option-as-Alt and notification alerts for this pane only. It does not install utilities or replace saved profiles.",
        "",
        "Without it2, or to make the settings permanent, follow these steps:",
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
            if !local_mac() { return; }
            let Some(id) = std::env::var("ITERM_SESSION_ID").ok()
                .and_then(|value| iterm_session_id(&value).map(str::to_string)) else { return };
            let help = setup_command("it2", &["profile", "session-set-property", "--help"]).await;
            if !help.is_ok_and(|text| text.contains("session-set-property")) {
                mode.show_status("Optional it2 automation unavailable. See Terminal setup above for installation and manual steps.");
                return;
            }
            if !mode.ask_confirm("Configure this iTerm2 pane?", "Use it2 to set both Option keys to Esc+ and enable escape-sequence notifications? Only this pane changes; saved profiles are untouched.").await { return; }
            match configure_it2_session(&id).await {
                Ok(()) => mode.show_status("iTerm2 pane configured: Option-as-Alt and terminal notifications enabled. No restart needed for these pane settings."),
                Err(error) => mode.show_error(&format!("iTerm2 pane setup failed: {error}")),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_targets_only_the_explicit_iterm_pane() {
        assert_eq!(iterm_session_id("w0t0p0:abc-123"), Some("abc-123"));
        assert_eq!(iterm_session_id("abc-123"), Some("abc-123"));
        assert_eq!(iterm_session_id(""), None);
        assert_eq!(iterm_session_id("w0t0p0:"), None);
        assert_eq!(iterm_session_id("--all"), None);
        assert_eq!(iterm_session_id("id with spaces"), None);
        assert_eq!(SESSION_PROPERTIES[0], ("Option Key Sends", "2"));
        assert_eq!(SESSION_PROPERTIES[3], ("Send Terminal Generated Alerts", "true"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn setup_command_reports_failure_instead_of_claiming_success() {
        assert_eq!(setup_command("sh", &["-c", "printf ready"]).await.unwrap(), "ready");
        let error = setup_command("sh", &["-c", "echo denied >&2; exit 1"]).await.unwrap_err();
        assert!(error.contains("denied"));
    }

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
