//! The tmux keyboard check at startup (upstream `checkTmuxKeyboardSetup`): Shift+Enter and
//! similar keys need `extended-keys` on, and work best with the `csi-u` format.

use std::time::Duration;

use super::mode::InteractiveMode;

const TMUX_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

const EXTENDED_KEYS_OFF: &str = "tmux extended-keys is off. Modified Enter keys may not work. Add `set -g extended-keys on` to ~/.tmux.conf and restart tmux."; // concise-ignore
const XTERM_FORMAT: &str = "tmux extended-keys-format is xterm. Pi works best with csi-u. Add `set -g extended-keys-format csi-u` to ~/.tmux.conf and restart tmux.";

/// The warning for the `extended-keys` and `extended-keys-format` values; none when tmux could
/// not be queried.
pub(super) fn tmux_keyboard_warning(extended_keys: Option<&str>, format: Option<&str>) -> Option<&'static str> {
    let extended_keys = extended_keys?;
    if extended_keys != "on" && extended_keys != "always" {
        return Some(EXTENDED_KEYS_OFF);
    }
    (format == Some("xterm")).then_some(XTERM_FORMAT)
}

/// Wraps an escape sequence for tmux, which passes it on with `set -g allow-passthrough on`.
pub(super) fn passthrough(sequence: &str, tmux: bool) -> String {
    if tmux { format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b")) } else { sequence.to_string() }
}

/// `tmux show -gv <option>`; `None` when it fails or takes longer than two seconds.
async fn tmux_show(option: &str) -> Option<String> {
    let mut command = tokio::process::Command::new("tmux");
    command.args(["show", "-gv", option]).stdin(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null()).kill_on_drop(true);
    let output = tokio::time::timeout(TMUX_QUERY_TIMEOUT, command.output()).await.ok()?.ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

impl InteractiveMode {
    /// Warn inside tmux when its keyboard setup drops Shift+Enter and similar keys.
    pub(super) fn check_tmux_keyboard_setup(&self) {
        if !self.uses_process_terminal || std::env::var_os("TMUX").is_none() {
            return;
        }
        self.spawn(|mode| async move {
            let query = async { tokio::join!(tmux_show("extended-keys"), tmux_show("extended-keys-format")) };
            let Ok((keys, format)) = tokio::spawn(query).await else { return };
            if let Some(warning) = tmux_keyboard_warning(keys.as_deref(), format.as_deref()) {
                mode.show_warning(warning);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_about_tmux_keyboard_settings() {
        assert_eq!(tmux_keyboard_warning(None, None), None);
        assert_eq!(tmux_keyboard_warning(Some("off"), None), Some(EXTENDED_KEYS_OFF));
        assert_eq!(tmux_keyboard_warning(Some("on"), Some("csi-u")), None);
        assert_eq!(tmux_keyboard_warning(Some("always"), None), None);
        assert_eq!(tmux_keyboard_warning(Some("on"), Some("xterm")), Some(XTERM_FORMAT));
    }

    #[test]
    fn wraps_sequences_for_tmux_passthrough() {
        assert_eq!(passthrough("\x1b]9;x\x07", true), "\x1bPtmux;\x1b\x1b]9;x\x07\x1b\\");
        assert_eq!(passthrough("\x1b]9;x\x07", false), "\x1b]9;x\x07");
        assert_eq!(passthrough("plain", true), "\x1bPtmux;plain\x1b\\");
    }
}
