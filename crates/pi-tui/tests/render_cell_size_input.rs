//! Port of upstream `test/tui-cell-size-input.test.ts`.

mod render_common;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::MutexGuard;

use pi_tui::env::{MapEnv, global_state_lock};
use pi_tui::terminal_image::{
    CellDimensions, get_capabilities_with, get_cell_dimensions, reset_capabilities_cache, set_cell_dimensions,
};
use pi_tui::testing::VirtualTerminal;
use pi_tui::{Component, TuiStopOptions, ui_loop};
use render_common::{erased, main_tui};

#[derive(Default)]
struct InputRecorder {
    inputs: Vec<String>,
}

impl Component for InputRecorder {
    fn render(&mut self, _width: usize) -> Vec<String> {
        vec![String::new()]
    }

    fn handle_input(&mut self, data: &str) {
        self.inputs.push(data.to_string());
    }

    fn handles_input(&self) -> bool {
        true
    }
}

/// Holds the global state lock with capabilities detected for Ghostty (Kitty images), and
/// restores the capability cache and cell size on drop.
struct ImageTerminalGuard {
    _lock: MutexGuard<'static, ()>,
}

impl Drop for ImageTerminalGuard {
    fn drop(&mut self) {
        reset_capabilities_cache();
        set_cell_dimensions(CellDimensions { width_px: 9, height_px: 18 });
    }
}

fn with_image_terminal() -> ImageTerminalGuard {
    let lock = global_state_lock();
    reset_capabilities_cache();
    let caps = get_capabilities_with(&MapEnv::new([("TERM_PROGRAM", "ghostty")]));
    assert!(caps.images.is_some(), "Ghostty should support images");
    ImageTerminalGuard { _lock: lock }
}

fn send(terminal: &Rc<RefCell<VirtualTerminal>>, data: &str) {
    terminal.borrow().send_input(data);
    ui_loop::run_pending().expect("input");
}

#[test]
fn forwards_bare_escape_even_when_a_cell_size_query_was_sent_at_startup() {
    let _guard = with_image_terminal();
    let terminal = VirtualTerminal::shared(80, 24);
    let tui = main_tui(&terminal);
    let recorder = Rc::new(RefCell::new(InputRecorder::default()));

    tui.set_focus(Some(erased(&recorder)));
    tui.start();

    send(&terminal, "\x1b");

    assert_eq!(recorder.borrow().inputs, vec!["\x1b".to_string()]);
    tui.stop(TuiStopOptions::default());
}

#[test]
fn consumes_cell_size_responses_and_still_forwards_later_user_input() {
    let _guard = with_image_terminal();
    set_cell_dimensions(CellDimensions { width_px: 9, height_px: 18 });

    let terminal = VirtualTerminal::shared(80, 24);
    let tui = main_tui(&terminal);
    let recorder = Rc::new(RefCell::new(InputRecorder::default()));

    tui.set_focus(Some(erased(&recorder)));
    tui.start();

    send(&terminal, "\x1b[6;20;10t");
    assert!(recorder.borrow().inputs.is_empty());
    assert_eq!(get_cell_dimensions(), CellDimensions { width_px: 10, height_px: 20 });

    send(&terminal, "q");
    assert_eq!(recorder.borrow().inputs, vec!["q".to_string()]);
    tui.stop(TuiStopOptions::default());
}
