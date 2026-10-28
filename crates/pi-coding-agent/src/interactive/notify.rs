//! The run-finished notification (pi-rust extension, after upstream `examples/extensions/notify.ts`).
//! A run that took at least `terminal.notifyMinSeconds` sends OSC 777, OSC 99, OSC 9, or BEL.

use pi_tui::env::{EnvLookup, ProcessEnv};

use super::mode::InteractiveMode;
use crate::resources::brand::APP_NAME;
use crate::settings::schema::NotifyOnFinish;

/// Longer bodies are cut to this many characters.
const MAX_BODY_CHARS: usize = 100;

/// How the terminal is told that a run finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyMethod {
    /// `OSC 777 ; notify ; title ; body`: Ghostty, WezTerm, foot, and rxvt-unicode.
    Osc777,
    /// `OSC 99`, title and body in two parts: kitty.
    Osc99,
    /// `OSC 9 ; message`: iTerm2 and Windows Terminal.
    Osc9,
    /// A plain BEL, which most terminals turn into a bell or an urgency hint.
    Bell,
}

/// The OSC notification protocol of the terminal in `env`; `None` for unknown terminals.
fn detect_osc(env: &dyn EnvLookup) -> Option<NotifyMethod> {
    let lower = |name: &str| env.var(name).unwrap_or_default().to_lowercase();
    let set = |name: &str| env.non_empty(name).is_some();
    let (program, term) = (lower("TERM_PROGRAM"), lower("TERM"));
    match program.as_str() {
        "kitty" => return Some(NotifyMethod::Osc99),
        "iterm.app" => return Some(NotifyMethod::Osc9),
        "ghostty" | "wezterm" => return Some(NotifyMethod::Osc777),
        _ => {}
    }
    if set("KITTY_WINDOW_ID") || term == "xterm-kitty" {
        Some(NotifyMethod::Osc99)
    } else if set("ITERM_SESSION_ID") || set("WT_SESSION") {
        Some(NotifyMethod::Osc9)
    } else if set("GHOSTTY_RESOURCES_DIR")
        || set("WEZTERM_PANE")
        || term.contains("ghostty")
        || term.starts_with("foot")
        || term.starts_with("rxvt")
    {
        Some(NotifyMethod::Osc777)
    } else {
        None
    }
}

fn in_multiplexer(env: &dyn EnvLookup) -> bool {
    let term = env.var("TERM").unwrap_or_default().to_lowercase();
    env.non_empty("TMUX").is_some()
        || env.non_empty("STY").is_some()
        || term.starts_with("tmux")
        || term.starts_with("screen")
}

/// The method for `setting` in `env`; `None` when notifications are off.
pub fn select_method(setting: NotifyOnFinish, env: &dyn EnvLookup) -> Option<NotifyMethod> {
    match setting {
        NotifyOnFinish::Off => None,
        NotifyOnFinish::Bell => Some(NotifyMethod::Bell),
        // tmux and screen drop OSC notifications unless wrapped for passthrough, but forward BEL.
        NotifyOnFinish::Auto if in_multiplexer(env) => Some(NotifyMethod::Bell),
        NotifyOnFinish::Auto => Some(detect_osc(env).unwrap_or(NotifyMethod::Bell)),
        NotifyOnFinish::Osc => Some(detect_osc(env).unwrap_or(NotifyMethod::Osc777)),
    }
}

/// `text` on one line without control characters, which would end the sequence early.
fn clean(text: &str, max_chars: usize) -> String {
    let line: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let line = line.trim();
    if line.chars().count() <= max_chars {
        return line.to_string();
    }
    let mut cut: String = line.chars().take(max_chars - 1).collect();
    cut.push('…');
    cut
}

/// The bytes that send `title` and `body` with `method`.
pub fn notification_sequence(method: NotifyMethod, title: &str, body: &str) -> String {
    let title = clean(title, MAX_BODY_CHARS).replace(';', ",");
    let body = clean(body, MAX_BODY_CHARS);
    match method {
        NotifyMethod::Osc777 => format!("\x1b]777;notify;{title};{body}\x07"),
        NotifyMethod::Osc99 => format!("\x1b]99;i=1:d=0;{title}\x1b\\\x1b]99;i=1:p=body;{body}\x1b\\"),
        NotifyMethod::Osc9 => format!("\x1b]9;{title}: {body}\x07"),
        NotifyMethod::Bell => "\x07".to_string(),
    }
}

impl InteractiveMode {
    /// After `AgentSettled`: notify when the run took long enough and the terminal is not
    /// known to have focus. Focus is known only in fullscreen mode with mouse support.
    pub(super) fn notify_run_finished(&self) {
        let Some(run) = self.program_status.borrow_mut().take_finished_run() else { return };
        let settings = self.settings();
        if run.elapsed < settings.notify_min_duration() || self.tui().terminal_focused() == Some(true) {
            return;
        }
        if let Some(method) = select_method(settings.notify_on_finish(), &ProcessEnv) {
            self.terminal.borrow_mut().write(&notification_sequence(method, APP_NAME, &run.summary));
        }
        self.iterm2_request_attention();
    }
}

#[cfg(test)]
mod tests {
    use pi_tui::env::MapEnv;

    use super::NotifyMethod::{Bell, Osc9, Osc99, Osc777};
    use super::*;

    fn method(setting: NotifyOnFinish, vars: &[(&str, &str)]) -> Option<NotifyMethod> {
        select_method(setting, &MapEnv::new(vars.iter().copied()))
    }

    #[test]
    fn auto_picks_the_protocol_of_the_terminal() {
        let cases: &[(&[(&str, &str)], NotifyMethod)] = &[
            (&[("TERM_PROGRAM", "ghostty")], Osc777),
            (&[("GHOSTTY_RESOURCES_DIR", "x")], Osc777),
            (&[("TERM_PROGRAM", "WezTerm")], Osc777),
            (&[("WEZTERM_PANE", "1")], Osc777),
            (&[("TERM", "foot")], Osc777),
            (&[("TERM", "rxvt-unicode-256color")], Osc777),
            (&[("KITTY_WINDOW_ID", "1")], Osc99),
            (&[("TERM", "xterm-kitty")], Osc99),
            (&[("TERM_PROGRAM", "iTerm.app")], Osc9),
            (&[("ITERM_SESSION_ID", "w0t0p0")], Osc9),
            (&[("WT_SESSION", "x")], Osc9),
            (&[("TERM", "xterm-256color")], Bell),
            (&[], Bell),
            // TERM_PROGRAM names the current terminal; other variables can leak from a parent.
            (&[("TERM_PROGRAM", "ghostty"), ("KITTY_WINDOW_ID", "1")], Osc777),
        ];
        for (vars, expected) in cases {
            assert_eq!(method(NotifyOnFinish::Auto, vars), Some(*expected), "{vars:?}");
        }
    }

    #[test]
    fn multiplexers_get_the_bell_unless_osc_is_forced() {
        for vars in [
            &[("TMUX", "x"), ("KITTY_WINDOW_ID", "1")][..],
            &[("TERM", "tmux-256color"), ("TERM_PROGRAM", "ghostty")],
            &[("STY", "x"), ("WT_SESSION", "x")],
            &[("TERM", "screen-256color")],
        ] {
            assert_eq!(method(NotifyOnFinish::Auto, vars), Some(Bell), "{vars:?}");
        }
        assert_eq!(method(NotifyOnFinish::Osc, &[("TMUX", "x"), ("KITTY_WINDOW_ID", "1")]), Some(Osc99));
        assert_eq!(method(NotifyOnFinish::Osc, &[("TMUX", "x")]), Some(Osc777));
    }

    #[test]
    fn explicit_settings_override_detection() {
        let kitty = [("KITTY_WINDOW_ID", "1")];
        assert_eq!(method(NotifyOnFinish::Off, &kitty), None);
        assert_eq!(method(NotifyOnFinish::Bell, &kitty), Some(Bell));
        assert_eq!(method(NotifyOnFinish::Osc, &kitty), Some(Osc99));
        assert_eq!(method(NotifyOnFinish::Osc, &[("TERM", "xterm-256color")]), Some(Osc777));
    }

    #[test]
    fn writes_the_exact_sequences() {
        assert_eq!(notification_sequence(Osc777, "pi", "Done"), "\x1b]777;notify;pi;Done\x07");
        assert_eq!(notification_sequence(Osc99, "pi", "Done"), "\x1b]99;i=1:d=0;pi\x1b\\\x1b]99;i=1:p=body;Done\x1b\\");
        assert_eq!(notification_sequence(Osc9, "pi", "Done"), "\x1b]9;pi: Done\x07");
        assert_eq!(notification_sequence(Bell, "pi", "Done"), "\x07");
    }

    #[test]
    fn bodies_cannot_end_the_sequence_and_stay_short() {
        let body = "rate limited\x07\x1b]0;evil\x1b\\ again";
        assert_eq!(
            notification_sequence(Osc777, "a;b", body),
            "\x1b]777;notify;a,b;rate limited  ]0;evil \\ again\x07"
        );
        let long = notification_sequence(Osc9, "pi", &"x".repeat(300));
        assert_eq!(long, format!("\x1b]9;pi: {}…\x07", "x".repeat(MAX_BODY_CHARS - 1)));
    }
}
