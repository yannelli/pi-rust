//! The theme controller driving a real `Tui`: terminal color queries, replies, and light/dark reports.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use pi_coding_agent::theme::{
    InteractiveThemeController, ThemeColor, ThemeControllerOptions, ThemeHost, set_theme_environment, theme,
};
use pi_tui::colors::{Color, color_to_hex};
use pi_tui::ui_loop::{self, TerminalSink};
use pi_tui::{Terminal, Tui, TuiOptions, TuiStopOptions};

#[derive(Default)]
struct TestTerminal {
    sink: Option<TerminalSink>,
    writes: Vec<String>,
}

impl Terminal for TestTerminal {
    fn start(&mut self, sink: TerminalSink) {
        self.sink = Some(sink);
    }

    fn stop(&mut self) {
        self.sink = None;
    }

    fn write(&mut self, data: &str) {
        self.writes.push(data.to_string());
    }

    fn columns(&self) -> usize {
        80
    }

    fn rows(&self) -> usize {
        24
    }

    fn kitty_protocol_active(&self) -> bool {
        false
    }

    fn move_by(&mut self, _lines: i32) {}
    fn hide_cursor(&mut self) {}
    fn show_cursor(&mut self) {}
    fn clear_line(&mut self) {}
    fn clear_from_cursor(&mut self) {}
    fn clear_screen(&mut self) {}
    fn set_title(&mut self, _title: &str) {}
}

fn send(terminal: &Rc<RefCell<TestTerminal>>, data: &str) {
    let sink = terminal.borrow().sink.clone().expect("terminal started");
    sink.input(data);
    ui_loop::run_pending().expect("ui loop");
}

/// Each reply as its own input sequence, as the terminal's stdin buffer splits them.
fn reply(terminal: &Rc<RefCell<TestTerminal>>, foreground: &str, background: &str) {
    send(terminal, &format!("\x1b]10;{foreground}\x07"));
    send(terminal, &format!("\x1b]11;{background}\x07"));
    for index in 0..16 {
        send(terminal, &format!("\x1b]4;{index};rgb:0000/0000/0000\x07"));
    }
    send(terminal, "\x1b[?62;22c");
}

#[test]
fn controller_queries_the_terminal_and_follows_its_appearance() {
    set_theme_environment(Some(Arc::new(pi_tui::env::MapEnv::default())));
    let terminal = Rc::new(RefCell::new(TestTerminal::default()));
    let tui = Tui::new_main_screen(terminal.clone(), TuiOptions::default());
    tui.start();
    let host: Rc<dyn ThemeHost> = Rc::new(tui.clone());
    let controller = InteractiveThemeController::new(
        host,
        ThemeControllerOptions {
            get_theme_setting: Box::new(|| Some("light/dark".to_string())),
            show_error: Box::new(|_| {}),
            on_changed: Box::new(|| {}),
            initial_theme_setting: None,
        },
    );
    controller.apply_from_settings();
    let written = terminal.borrow().writes.concat();
    assert!(written.contains("\x1b]11;?\x07"), "{written:?}");
    assert!(written.contains("\x1b[?2031h"), "{written:?}");
    assert!(!controller.terminal_colors_settled());

    reply(&terminal, "rgb:1e1e/1e1e/1e1e", "rgb:fafa/fafa/fafa");
    assert!(controller.terminal_colors_settled());
    assert_eq!(theme().name(), Some("light"));

    // A dark report re-queries the colors, and the reply decides the appearance.
    terminal.borrow_mut().writes.clear();
    send(&terminal, "\x1b[?997;1n");
    assert!(terminal.borrow().writes.concat().contains("\x1b]11;?\x07"));
    reply(&terminal, "rgb:f8f8/f8f8/f2f2", "rgb:2828/2a2a/3636");
    assert_eq!(theme().name(), Some("dark"));

    // The system theme takes its colors from the terminal.
    controller.set_theme_name("system", false).expect("system theme");
    assert!(matches!(theme().color(ThemeColor::Error), Color::Rgb { .. }));
    assert_eq!(color_to_hex(&theme().color(pi_coding_agent::theme::ThemeBg::UserMessageBg)).len(), 7);

    controller.dispose();
    assert!(terminal.borrow().writes.concat().contains("\x1b[?2031l"));
    tui.stop(TuiStopOptions::default());
}
