//! Port of upstream `tui-alt-screen.test.ts`: mouse tracking modes, right-click paste, and
//! scrollbar hover and dragging.

mod alt_view_common;
mod layout_common;
mod support;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use alt_view_common::{Harness, numbered_lines, scroll_view, wait_ms};
use layout_common::text;
use pi_tui::env::{EnvLookup, MapEnv};
use pi_tui::{ScrollViewOptions, ScrollViewScrollbar, TuiAltScreenOptions};

/// Upstream starts a `TuiAltScreen` on a `RecordingTerminal` and joins its writes.
fn start_writes(environment: &[(&str, &str)]) -> String {
    let env: Rc<dyn EnvLookup> = Rc::new(MapEnv::new(environment.iter().copied()));
    let h = Harness::with_env(80, 24, env, TuiAltScreenOptions::default());
    h.tui.start();
    let writes = h.terminal.borrow().writes();
    h.stop();
    writes
}

#[test]
fn uses_button_motion_tracking_inside_terminal_multiplexers() {
    let direct_writes = start_writes(&[("TERM", "xterm-256color")]);
    assert!(direct_writes.contains("\x1b[?1003h"));

    let multiplexers: [(&str, &[(&str, &str)]); 5] = [
        ("tmux environment", &[("TMUX", "/tmp/tmux/default,1,0")]),
        ("tmux TERM", &[("TERM", "tmux-256color")]),
        ("Zellij environment", &[("ZELLIJ", "0")]),
        ("Screen environment", &[("STY", "123.session")]),
        ("Screen TERM", &[("TERM", "screen-256color")]),
    ];
    for (name, environment) in multiplexers {
        let writes = start_writes(environment);
        assert!(writes.contains("\x1b[?1002h"), "{name} should enable button-motion tracking");
        assert!(!writes.contains("\x1b[?1003h"), "{name} should not enable all-motion tracking");
        assert!(writes.contains("\x1b[?1006h"), "{name} should enable SGR mouse encoding");
    }
}

/// Upstream switches `process.platform` between win32 and linux. The Rust port checks the
/// platform at compile time, so the expected count follows the platform the test runs on.
#[test]
fn invokes_the_right_click_paste_handler_only_on_windows_outside_vs_code() {
    let variables: Rc<RefCell<HashMap<String, String>>> = Rc::default();
    let lookup = variables.clone();
    let env: Rc<dyn EnvLookup> = Rc::new(move |name: &str| lookup.borrow().get(name).cloned());
    let paste_count = Rc::new(Cell::new(0));
    let counter = paste_count.clone();
    let options = TuiAltScreenOptions {
        on_right_click_paste: Some(Rc::new(move || counter.set(counter.get() + 1))),
        ..TuiAltScreenOptions::default()
    };
    let h = Harness::with_env(80, 24, env, options);
    let windows_count = if cfg!(windows) { 1 } else { 0 };

    h.tui.start();
    h.send("\x1b[<2;1;1M");
    h.send("\x1b[<2;1;1m");
    assert_eq!(paste_count.get(), windows_count);

    variables.borrow_mut().insert("TERM_PROGRAM".into(), "vscode".into());
    h.send("\x1b[<2;1;1M");
    assert_eq!(paste_count.get(), windows_count);

    variables.borrow_mut().remove("TERM_PROGRAM");
    if !cfg!(windows) {
        h.send("\x1b[<2;1;1M");
        assert_eq!(paste_count.get(), windows_count);
    }
    h.stop();
}

#[test]
fn reveals_an_auto_scrollbar_when_the_pointer_enters_its_hidden_track() {
    let h = Harness::new(10, 5);
    let options = ScrollViewOptions {
        primary: true,
        scrollbar: ScrollViewScrollbar::Auto,
        scrollbar_hide_delay_ms: Some(20),
        ..ScrollViewOptions::default()
    };
    let (view, state) = scroll_view(text(&numbered_lines(20)), options);
    h.tui.set_layout_root(Some(view));
    h.tui.start();
    h.wait();
    assert!(!state.is_scrollbar_visible());

    h.send("\x1b[<35;10;3M");
    h.wait();
    assert!(state.is_scrollbar_visible());
    assert!(state.is_scrollbar_active());
    assert!(h.viewport().iter().any(|line| line.contains(['│', '█'])));

    h.send("\x1b[<35;9;3M");
    wait_ms(40);
    h.wait();
    assert!(!state.is_scrollbar_visible());
    h.stop();
}

#[test]
fn jumps_to_a_scrollbar_track_position_and_continues_dragging_from_there() {
    let h = Harness::new(10, 10);
    let options =
        ScrollViewOptions { primary: true, scrollbar: ScrollViewScrollbar::Always, ..ScrollViewOptions::default() };
    let (view, state) = scroll_view(text(&numbered_lines(50)), options);
    h.tui.set_layout_root(Some(view));
    h.tui.start();
    h.wait();
    assert_eq!(state.scroll_top(), 0);

    h.send("\x1b[<0;10;6M");
    h.wait();
    assert_eq!(state.scroll_top(), 20);

    h.send("\x1b[<32;10;10M");
    h.wait();
    assert_eq!(state.scroll_top(), 40);

    h.send("\x1b[<0;10;10m");
    h.wait();
    assert!(!h.wrote("\x1b]52;c;"));
    h.stop();
}
