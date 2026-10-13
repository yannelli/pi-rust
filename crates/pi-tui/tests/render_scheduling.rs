//! Port of the render scheduling, debug logging, bounded output, and crash dump cases of upstream
//! `test/tui-render.test.ts`.

mod render_common;

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::testing::VirtualTerminal;
use pi_tui::{Component, Terminal, TerminalSink, Tui, TuiOptions, TuiStopOptions, ui_loop};
use render_common::{TestComponent, erased, main_tui, options_with_env, strings, test_component, wait_for_render};

const MAX_RENDER_WRITE_CHARS: usize = 1024 * 1024;

/// Upstream `InputComponent`: counts renders and shows the last input.
#[derive(Default)]
struct InputComponent {
    lines: Vec<String>,
    render_count: usize,
}

impl Component for InputComponent {
    fn render(&mut self, _width: usize) -> Vec<String> {
        self.render_count += 1;
        self.lines.clone()
    }

    fn handle_input(&mut self, data: &str) {
        self.lines = vec![data.to_string()];
    }

    fn handles_input(&self) -> bool {
        true
    }
}

/// Upstream `BoundedWriteTerminal`: records writes; every other terminal call does nothing.
#[derive(Default)]
struct BoundedWriteTerminal {
    writes: Vec<String>,
}

impl Terminal for BoundedWriteTerminal {
    fn start(&mut self, _sink: TerminalSink) {}
    fn stop(&mut self) {}
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

fn kitty_line() -> String {
    format!("\x1b_Ga=T,f=100;{}\x1b\\", "A".repeat(1_200_000))
}

#[test]
fn renders_keyboard_input_without_waiting_for_a_throttled_frame() {
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    let component = Rc::new(RefCell::new(InputComponent { lines: strings(&["initial"]), render_count: 0 }));
    tui.add_child(erased(&component));
    tui.set_focus(Some(erased(&component)));
    tui.start();
    tui.render_now(false).expect("render");
    let render_count_before_input = component.borrow().render_count;

    // Queue a throttled render first. Keyboard input should preempt it.
    component.borrow_mut().lines = strings(&["pending"]);
    tui.request_render(false);
    terminal.borrow().send_input("first");
    terminal.borrow().send_input("second");
    terminal.borrow().send_input("typed");
    ui_loop::run_pending().expect("render");

    assert_eq!(component.borrow().render_count, render_count_before_input + 1);
    assert_eq!(component.borrow().lines, strings(&["typed"]));
    tui.stop(TuiStopOptions::default());
}

#[test]
fn writes_redraw_logs_to_the_provided_directory() {
    let log_dir = tempfile::Builder::new().prefix("pi-tui-log-").tempdir().expect("temp dir");
    let terminal = VirtualTerminal::shared(40, 10);
    let options = TuiOptions {
        log_directory: Some(log_dir.path().to_path_buf()),
        ..options_with_env(&[("PI_TUI_DEBUG_REDRAW", "1")])
    };
    let tui = Tui::new_main_screen(terminal.clone(), options);
    let component = test_component(&[]);
    tui.add_child(erased(&component));
    component.borrow_mut().lines = strings(&["test"]);
    tui.start();
    wait_for_render();

    let log = std::fs::read_to_string(log_dir.path().join("pi-tui-debug.log")).expect("debug log");
    assert!(log.contains("fullRender: first render"), "{log}");
    tui.stop(TuiStopOptions::default());
}

#[test]
fn splits_a_large_full_render_without_changing_its_output() {
    let terminal = Rc::new(RefCell::new(BoundedWriteTerminal::default()));
    let tui = main_tui(&terminal);
    let component = test_component(&[]);
    let kitty_line = kitty_line();
    component.borrow_mut().lines = vec![kitty_line.clone(), kitty_line.clone()];
    tui.add_child(erased(&component));

    tui.render_now(false).expect("render");

    let writes = &terminal.borrow().writes;
    assert!(writes.len() > 2, "large output should be split across terminal writes");
    assert!(
        writes.iter().all(|write| write.len() <= MAX_RENDER_WRITE_CHARS),
        "each terminal write should stay below the configured limit"
    );
    assert!(
        writes.concat() == format!("\x1b[?2026h{kitty_line}\r\n{kitty_line}\x1b[?2026l"),
        "chunking must preserve the synchronized render output"
    );
}

#[test]
fn splits_large_differential_updates_without_a_full_redraw() {
    let terminal = Rc::new(RefCell::new(BoundedWriteTerminal::default()));
    let tui = main_tui(&terminal);
    let component = test_component(&[]);
    tui.add_child(erased(&component));
    component.borrow_mut().lines = strings(&["before"]);
    tui.render_now(false).expect("render");
    terminal.borrow_mut().writes.clear();

    let kitty_line = kitty_line();
    component.borrow_mut().lines = vec!["before".to_string(), kitty_line.clone(), kitty_line];
    tui.render_now(false).expect("render");

    let writes = &terminal.borrow().writes;
    assert!(writes.len() > 2, "large output should be split across terminal writes");
    assert!(writes.iter().all(|write| write.len() <= MAX_RENDER_WRITE_CHARS));
    let output = writes.concat();
    assert!(output.starts_with("\x1b[?2026h"));
    assert!(output.ends_with("\x1b[?2026l"));
    assert!(!output.contains("\x1b[2J"), "the update should stay on the differential render path");
}

#[test]
fn writes_the_crash_dump_to_the_os_temp_directory_instead_of_a_home_directory_default() {
    // The TUI falls back to the temp directory when no log directory is set. The temp directory
    // comes from TMPDIR (TEMP and TMP on Windows), so the test points all three at a fresh one.
    let crash_dir = tempfile::Builder::new().prefix("pi-tui-crash-").tempdir().expect("temp dir");
    let crash_log_path = crash_dir.path().join("pi-tui-crash.log");
    let dir = crash_dir.path().to_str().expect("utf-8 path");
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = Tui::new_main_screen(terminal.clone(), options_with_env(&[("TMPDIR", dir), ("TEMP", dir), ("TMP", dir)]));
    let component = Rc::new(RefCell::new(TestComponent::default()));
    tui.add_child(erased(&component));
    component.borrow_mut().lines = strings(&["ok"]);
    tui.start();
    wait_for_render();

    // Width overflow is detected in the differential render path.
    component.borrow_mut().lines = vec!["ok".to_string(), "x".repeat(60)];
    let error = tui.render_now(false).expect_err("overflow should fail the render");
    let message = error.to_string();
    let expected_path = crash_log_path.display().to_string();
    assert!(message.contains(&expected_path), "error message should reference {expected_path}: {message}");
    let crash_log = std::fs::read_to_string(&crash_log_path).expect("crash log");
    assert!(crash_log.contains("Terminal width: 40"), "{crash_log}");
}
