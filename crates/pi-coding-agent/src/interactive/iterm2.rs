//! Optional iTerm2 integration (pi-rust extension, `terminal.iterm2`): the tab color follows the
//! agent state, a badge names the session, user variables feed iTerm2 status bar components, a
//! finished run requests attention, and iTerm2 learns the cwd. `/terminal-setup` turns it on.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use pi_tui::env::{EnvLookup, ProcessEnv};
use pi_tui::program_status::{ProgramState, ProgramStatus};

use super::mode::InteractiveMode;
use crate::settings::schema::ITerm2Features;

// iTerm2 picks tab-title ink at a brightness cutoff, not WCAG contrast. Keep backgrounds
// light enough for black titles, including its inactive-tab blend toward middle gray.
const WORKING: [u8; 3] = [132, 184, 255];
const BLOCKED: [u8; 3] = [242, 202, 114];
const DONE: [u8; 3] = [151, 211, 163];
const ERROR: [u8; 3] = [245, 160, 164];

thread_local! {
    /// The last value written per key, so an unchanged value is not sent again.
    static SENT: RefCell<HashMap<&'static str, String>> = RefCell::new(HashMap::new());
}

/// iTerm2 sets `TERM_PROGRAM`; `LC_TERMINAL` also reaches tmux and SSH sessions.
pub fn is_iterm2(env: &dyn EnvLookup) -> bool {
    env.var("TERM_PROGRAM").is_some_and(|program| program == "iTerm.app")
        || env.var("LC_TERMINAL").is_some_and(|terminal| terminal == "iTerm2")
        || env.non_empty("ITERM_SESSION_ID").is_some()
}

/// Wraps an escape sequence for tmux, which passes it on with `set -g allow-passthrough on`.
pub fn passthrough(sequence: &str, tmux: bool) -> String {
    if tmux { format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b")) } else { sequence.to_string() }
}

/// `OSC 6` tab color; `None` restores the profile color.
pub fn tab_color(rgb: Option<[u8; 3]>) -> String {
    match rgb {
        Some([r, g, b]) => format!(
            "\x1b]6;1;bg;red;brightness;{r}\x07\x1b]6;1;bg;green;brightness;{g}\x07\x1b]6;1;bg;blue;brightness;{b}\x07"
        ),
        None => "\x1b]6;1;bg;*;default\x07".to_string(),
    }
}

/// `OSC 1337 SetUserVar`; iTerm2 shows it as `\(user.<name>)`.
pub fn user_var(name: &str, value: &str) -> String {
    format!("\x1b]1337;SetUserVar={name}={}\x07", STANDARD.encode(value))
}

/// `OSC 1337 SetBadgeFormat`; an empty text removes the badge.
pub fn badge(text: &str) -> String {
    format!("\x1b]1337;SetBadgeFormat={}\x07", STANDARD.encode(text))
}

pub fn request_attention() -> &'static str {
    "\x1b]1337;RequestAttention=yes\x07"
}

pub fn current_dir(path: &Path) -> String {
    let path: String = path.display().to_string().chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]1337;CurrentDir={path}\x07")
}

/// The tab color for a state; idle and cleared states use the profile color.
pub fn state_color(state: ProgramState) -> Option<[u8; 3]> {
    match state {
        ProgramState::Working => Some(WORKING),
        ProgramState::Blocked => Some(BLOCKED),
        ProgramState::Done => Some(DONE),
        ProgramState::Error => Some(ERROR),
        _ => None,
    }
}

pub fn state_name(state: ProgramState) -> &'static str {
    match state {
        ProgramState::Idle | ProgramState::Clear => "idle",
        ProgramState::Working => "working",
        ProgramState::Blocked => "blocked",
        ProgramState::Done => "done",
        ProgramState::Error => "error",
    }
}

impl InteractiveMode {
    /// The enabled features, or `None` outside iTerm2 or when the setting is off.
    fn iterm2(&self) -> Option<(ITerm2Features, bool)> {
        let features = self.settings().iterm2_features();
        let enabled = self.uses_process_terminal && features.any() && is_iterm2(&ProcessEnv);
        enabled.then(|| (features, ProcessEnv.non_empty("TMUX").is_some()))
    }

    fn iterm2_write(&self, key: &'static str, sequence: String, tmux: bool) {
        let changed = SENT.with(|sent| sent.borrow_mut().insert(key, sequence.clone()) != Some(sequence.clone()));
        if changed {
            self.terminal.borrow_mut().write(&passthrough(&sequence, tmux));
        }
    }

    /// After a program status change: the tab color and `user.pi_state`.
    pub(super) fn iterm2_report_state(&self, status: &ProgramStatus) {
        let Some((features, tmux)) = self.iterm2() else { return };
        if features.tab_color {
            self.iterm2_write("tab", tab_color(state_color(status.state)), tmux);
        }
        if features.user_vars {
            self.iterm2_write("pi_state", user_var("pi_state", state_name(status.state)), tmux);
        }
        self.iterm2_report_session();
    }

    /// After the session or its name changed: the badge, the cwd, and the session user variables.
    pub(super) fn iterm2_report_session(&self) {
        let Some((features, tmux)) = self.iterm2() else { return };
        let session = self.session();
        let project = session.cwd().file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let name = session.session_name().unwrap_or_else(|| project.clone());
        if features.badge {
            self.iterm2_write("badge", badge(&format!("π {name}")), tmux);
        }
        if features.cwd {
            self.iterm2_write("cwd", current_dir(session.cwd()), tmux);
        }
        if features.user_vars {
            let model = session.model().map(|model| model.id).unwrap_or_default();
            self.iterm2_write("pi_session", user_var("pi_session", &name), tmux);
            self.iterm2_write("pi_project", user_var("pi_project", &project), tmux);
            self.iterm2_write("pi_model", user_var("pi_model", &model), tmux);
        }
    }

    /// When a long run finished and the terminal is not known to have focus.
    pub(super) fn iterm2_request_attention(&self) {
        if let Some((_, tmux)) = self.iterm2().filter(|(features, _)| features.attention) {
            self.terminal.borrow_mut().write(&passthrough(request_attention(), tmux));
        }
    }

    /// On exit: the profile tab color and no badge.
    pub(super) fn iterm2_reset(&self) {
        let Some((features, tmux)) = self.iterm2() else { return };
        if features.tab_color {
            self.terminal.borrow_mut().write(&passthrough(&tab_color(None), tmux));
        }
        if features.badge {
            self.terminal.borrow_mut().write(&passthrough(&badge(""), tmux));
        }
        SENT.with(|sent| sent.borrow_mut().clear());
    }
}

#[cfg(test)]
mod tests {
    use pi_tui::env::MapEnv;

    use super::*;

    #[test]
    fn detects_iterm2_directly_and_through_tmux_or_ssh() {
        let env = |vars: &[(&str, &str)]| MapEnv::new(vars.iter().copied());
        assert!(is_iterm2(&env(&[("TERM_PROGRAM", "iTerm.app")])));
        assert!(is_iterm2(&env(&[("TERM_PROGRAM", "tmux"), ("LC_TERMINAL", "iTerm2")])));
        assert!(is_iterm2(&env(&[("ITERM_SESSION_ID", "w0t0p0:1")])));
        assert!(!is_iterm2(&env(&[("TERM_PROGRAM", "ghostty")])));
        assert!(!is_iterm2(&env(&[])));
    }

    #[test]
    fn state_colors_keep_black_titles_readable_when_tabs_are_dimmed() {
        let linear = |value: f64| {
            if value <= 0.04045 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
        };
        for state in [ProgramState::Working, ProgramState::Blocked, ProgramState::Done, ProgramState::Error] {
            let rgb = state_color(state).expect("colored state");
            // Selected/unselected tabs in active/inactive windows use these blends in iTerm2.
            for alpha in [1.0, 0.6, 0.4, 0.3] {
                let [r, g, b] = rgb.map(|c| f64::from(c) / 255.0 * alpha + 0.5 * (1.0 - alpha));
                assert!(0.299 * r + 0.587 * g + 0.114 * b > 0.5, "{state:?}, alpha {alpha}");
                let luminance = 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
                assert!((luminance + 0.05) / 0.05 >= 4.5, "{state:?}, alpha {alpha}");
            }
        }
        assert_eq!(state_color(ProgramState::Idle), None);
        assert_eq!(state_color(ProgramState::Clear), None);
    }

    #[test]
    fn builds_iterm2_sequences() {
        assert_eq!(
            tab_color(Some([1, 2, 3])),
            "\x1b]6;1;bg;red;brightness;1\x07\x1b]6;1;bg;green;brightness;2\x07\x1b]6;1;bg;blue;brightness;3\x07"
        );
        assert_eq!(tab_color(None), "\x1b]6;1;bg;*;default\x07");
        assert_eq!(user_var("pi_state", "working"), "\x1b]1337;SetUserVar=pi_state=d29ya2luZw==\x07");
        assert_eq!(badge("π x"), format!("\x1b]1337;SetBadgeFormat={}\x07", STANDARD.encode("π x")));
        assert_eq!(badge(""), "\x1b]1337;SetBadgeFormat=\x07");
        assert_eq!(current_dir(Path::new("/w/a\nb")), "\x1b]1337;CurrentDir=/w/ab\x07");
        assert_eq!(passthrough("\x1b]9;x\x07", true), "\x1bPtmux;\x1b\x1b]9;x\x07\x1b\\");
        assert_eq!(passthrough("\x1b]9;x\x07", false), "\x1b]9;x\x07");
        assert_eq!(state_color(ProgramState::Working), Some(WORKING));
        assert_eq!(state_color(ProgramState::Idle), None);
        assert_eq!(state_name(ProgramState::Blocked), "blocked");
    }
}
