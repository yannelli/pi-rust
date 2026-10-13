//! The interactive theme controller (upstream `theme-controller.test.ts`), driven by a fake TUI host.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use pi_tui::colors::Color;
use pi_tui::terminal_colors::{RgbColor, TerminalColors};

use super::*;

fn dark() -> TerminalColors {
    TerminalColors {
        foreground: Some(RgbColor::new(248, 248, 242)),
        background: Some(RgbColor::new(40, 42, 54)),
        palette: None,
    }
}

fn light() -> TerminalColors {
    TerminalColors {
        foreground: Some(RgbColor::new(30, 30, 30)),
        background: Some(RgbColor::new(250, 250, 250)),
        palette: None,
    }
}

type Reply = Box<dyn FnOnce(TerminalColors)>;
type SchemeListener = Box<dyn FnMut(TerminalTheme)>;

/// How the fake terminal answers color queries.
enum Answer {
    Now(TerminalColors),
    Later,
    TimeoutThenLate,
}

struct FakeHost {
    answer: RefCell<Answer>,
    queries: Cell<usize>,
    pending: RefCell<Option<Reply>>,
    late: RefCell<Option<Reply>>,
    notifications: RefCell<Vec<bool>>,
    listener: RefCell<Option<SchemeListener>>,
    unsubscribed: Rc<Cell<usize>>,
    renders: Cell<usize>,
}

impl FakeHost {
    fn new(answer: Answer) -> Rc<FakeHost> {
        Rc::new(FakeHost {
            answer: RefCell::new(answer),
            queries: Cell::new(0),
            pending: RefCell::new(None),
            late: RefCell::new(None),
            notifications: RefCell::new(Vec::new()),
            listener: RefCell::new(None),
            unsubscribed: Rc::new(Cell::new(0)),
            renders: Cell::new(0),
        })
    }

    fn emit_scheme(&self, scheme: TerminalTheme) {
        let mut listener = self.listener.borrow_mut().take().expect("listener");
        listener(scheme);
        *self.listener.borrow_mut() = Some(listener);
    }
}

impl ThemeHost for FakeHost {
    fn query_terminal_colors(&self, _timeout: Duration, on_result: Reply, on_late_reply: Reply) {
        self.queries.set(self.queries.get() + 1);
        let answer = std::mem::replace(&mut *self.answer.borrow_mut(), Answer::Later);
        match answer {
            Answer::Now(colors) => {
                *self.answer.borrow_mut() = Answer::Now(colors.clone());
                on_result(colors);
            }
            Answer::Later => *self.pending.borrow_mut() = Some(on_result),
            Answer::TimeoutThenLate => {
                *self.late.borrow_mut() = Some(on_late_reply);
                on_result(TerminalColors::default());
            }
        }
    }

    fn set_terminal_color_scheme_notifications(&self, enabled: bool) {
        self.notifications.borrow_mut().push(enabled);
    }

    fn on_terminal_color_scheme_change(&self, listener: Box<dyn FnMut(TerminalTheme)>) -> Box<dyn FnOnce()> {
        *self.listener.borrow_mut() = Some(listener);
        let unsubscribed = self.unsubscribed.clone();
        Box::new(move || unsubscribed.set(unsubscribed.get() + 1))
    }

    fn invalidate(&self) {}

    fn request_render(&self) {
        self.renders.set(self.renders.get() + 1);
    }
}

fn controller(
    host: &Rc<FakeHost>,
    setting: &Rc<RefCell<Option<String>>>,
    initial: Option<&str>,
) -> InteractiveThemeController {
    let setting = setting.clone();
    InteractiveThemeController::new(
        host.clone(),
        ThemeControllerOptions {
            get_theme_setting: Box::new(move || setting.borrow().clone()),
            show_error: Box::new(|_| {}),
            on_changed: Box::new(|| {}),
            initial_theme_setting: initial.map(String::from),
        },
    )
}

fn saved(value: Option<&str>) -> Rc<RefCell<Option<String>>> {
    Rc::new(RefCell::new(value.map(String::from)))
}

#[test]
fn uses_the_initial_theme_without_persisting_it() {
    let _state = global_state();
    let host = FakeHost::new(Answer::Now(TerminalColors::default()));
    let controller = controller(&host, &saved(Some("dark")), Some("light"));
    assert_eq!(theme().name(), Some("light"));
    assert_eq!(controller.get_theme_selection().as_deref(), Some("light"));
    controller.apply_from_settings();
    assert_eq!(host.queries.get(), 1);
}

#[test]
fn applies_the_theme_at_once_and_lets_startup_wait_for_the_colors() {
    let _state = global_state();
    let host = FakeHost::new(Answer::Later);
    let controller = controller(&host, &saved(None), None);
    controller.apply_from_settings();
    assert_eq!(theme().name(), Some("system"));
    assert_eq!(theme().get_fg_ansi(ThemeColor::Error), "\x1b[39m");
    assert!(!controller.terminal_colors_settled());
    let answer = host.pending.borrow_mut().take().expect("pending query");
    answer(dark());
    futures::executor::block_on(controller.wait_for_terminal_colors());
    assert!(theme().get_fg_ansi(ThemeColor::Error).starts_with("\x1b[38;"));
}

#[test]
fn falls_back_to_palette_indices_then_applies_late_colors() {
    let _state = global_state();
    let host = FakeHost::new(Answer::TimeoutThenLate);
    let controller = controller(&host, &saved(None), None);
    controller.apply_from_settings();
    assert_eq!(theme().get_fg_ansi(ThemeColor::Error), "\x1b[38;5;1m");
    let late = host.late.borrow_mut().take().expect("late reply");
    late(dark());
    assert!(matches!(theme().color(ThemeColor::Error), Color::Rgb { .. }));
}

#[test]
fn re_queries_colors_on_appearance_changes_and_lets_them_decide() {
    let _state = global_state();
    let host = FakeHost::new(Answer::Now(light()));
    let controller = controller(&host, &saved(None), Some("light/dark"));
    controller.apply_from_settings();
    assert_eq!(host.notifications.borrow().last(), Some(&true));
    assert_eq!(theme().name(), Some("light"));
    *host.answer.borrow_mut() = Answer::Now(dark());
    // The report says light, but the terminal renders dark.
    host.emit_scheme(TerminalTheme::Light);
    assert_eq!(theme().name(), Some("dark"));
}

#[test]
fn uses_the_reported_scheme_for_the_system_theme_without_colors() {
    let _state = global_state();
    let host = FakeHost::new(Answer::Now(TerminalColors::default()));
    let controller = controller(&host, &saved(None), None);
    controller.apply_from_settings();
    assert_eq!(theme().appearance(), TerminalTheme::Dark);
    host.emit_scheme(TerminalTheme::Light);
    assert_eq!(theme().appearance(), TerminalTheme::Light);
    assert_eq!(controller.get_terminal_theme(), TerminalTheme::Light);
}

#[test]
fn re_renders_only_when_the_reported_colors_change() {
    let _state = global_state();
    let host = FakeHost::new(Answer::Now(TerminalColors::default()));
    let controller = controller(&host, &saved(Some("dark")), None);
    for colors in [dark(), TerminalColors::default(), dark()] {
        *host.answer.borrow_mut() = Answer::Now(colors);
        controller.apply_from_settings();
    }
    assert_eq!(host.renders.get(), 1);
}

#[test]
fn disables_appearance_updates_when_disposed() {
    let _state = global_state();
    let host = FakeHost::new(Answer::Now(TerminalColors::default()));
    let controller = controller(&host, &saved(Some("light/dark")), None);
    controller.apply_from_settings();
    controller.dispose();
    assert_eq!(host.notifications.borrow().last(), Some(&false));
    assert_eq!(host.unsubscribed.get(), 1);
}

#[test]
fn an_explicit_selection_replaces_the_initial_theme() {
    let _state = global_state();
    let host = FakeHost::new(Answer::Now(TerminalColors::default()));
    let setting = saved(Some("dark"));
    let controller = controller(&host, &setting, Some("light"));
    controller.apply_from_settings();
    assert!(controller.set_theme_name("dark", false).is_ok());
    *setting.borrow_mut() = Some("light".into());
    controller.apply_from_settings();
    assert_eq!(controller.get_theme_selection().as_deref(), Some("dark"));
    assert_eq!(theme().name(), Some("dark"));
}

#[test]
fn reloads_theme_settings_when_no_initial_theme_was_given() {
    let _state = global_state();
    let host = FakeHost::new(Answer::Now(TerminalColors::default()));
    let setting = saved(Some("dark"));
    let controller = controller(&host, &setting, None);
    controller.apply_from_settings();
    *setting.borrow_mut() = Some("light".into());
    controller.apply_from_settings();
    assert_eq!(theme().name(), Some("light"));
    *setting.borrow_mut() = Some("dark".into());
    controller.apply_from_settings();
    assert_eq!(theme().name(), Some("dark"));
}

#[test]
fn in_memory_themes_stay_active_across_terminal_updates() {
    let _state = global_state();
    let host = FakeHost::new(Answer::Now(dark()));
    let controller = controller(&host, &saved(None), None);
    controller.apply_from_settings();
    let custom = std::sync::Arc::new(load_edited("light", |json| json["name"] = serde_json::json!("custom")));
    controller.select(ThemeSelection::Instance(custom)).expect("instance");
    *host.answer.borrow_mut() = Answer::Now(light());
    controller.apply_from_settings();
    assert_eq!(theme().name(), Some("system"));
    controller.select(ThemeSelection::Instance(std::sync::Arc::new(load_edited("dark", |_| {})))).expect("instance");
    host.emit_scheme(TerminalTheme::Light);
    assert_eq!(theme().name(), Some("dark"));
    assert!(controller.select(ThemeSelection::Name("missing".into())).is_err());
    assert_eq!(theme().name(), Some("system"));
}
