//! Port of upstream `test/terminal.test.ts`, "ProcessTerminal progress", "ProcessTerminal title", and
//! "ProcessTerminal dimensions". The backend `write` and `size` hooks replace `process.stdout`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_tui::env::MapEnv;
use pi_tui::process_terminal::{ProcessTerminal, ProcessTerminalBackend};
use pi_tui::terminal::Terminal;
use pi_tui::ui_loop;

const PROGRESS_ACTIVE: &str = "\x1b]9;4;3\x07";
const PROGRESS_CLEAR: &str = "\x1b]9;4;0\x07";

fn terminal_with(env: MapEnv, size: Option<(usize, usize)>) -> (ProcessTerminal, Arc<Mutex<Vec<String>>>) {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let sink = writes.clone();
    let terminal = ProcessTerminal::with_backend(ProcessTerminalBackend {
        env: Arc::new(env),
        write: Box::new(move |data: &str| sink.lock().unwrap().push(data.to_string())),
        size: Box::new(move || size),
        real_io: false,
    });
    (terminal, writes)
}

#[test]
fn writes_a_valid_osc_9_4_clear_sequence() {
    let (mut terminal, writes) = terminal_with(MapEnv::default(), Some((80, 24)));
    terminal.set_progress(false);
    assert_eq!(*writes.lock().unwrap(), vec![PROGRESS_CLEAR.to_string()]);
}

/// Not an upstream case: covers the 1 s keepalive interval and its cleanup on stop.
#[test]
fn repeats_the_active_progress_sequence_every_second_until_stopped() {
    let (mut terminal, writes) = terminal_with(MapEnv::default(), Some((80, 24)));
    terminal.set_progress(true);
    terminal.set_progress(true);
    assert_eq!(*writes.lock().unwrap(), vec![PROGRESS_ACTIVE.to_string(); 2]);

    ui_loop::run_for(Duration::from_millis(1100)).unwrap();
    assert_eq!(*writes.lock().unwrap(), vec![PROGRESS_ACTIVE.to_string(); 3]);

    terminal.stop();
    let after_stop = writes.lock().unwrap().clone();
    assert_eq!(after_stop[3], PROGRESS_CLEAR);
    ui_loop::run_for(Duration::from_millis(1100)).unwrap();
    assert_eq!(*writes.lock().unwrap(), after_stop);
}

#[test]
fn replaces_control_characters_so_the_title_cannot_end_the_osc_sequence() {
    let (mut terminal, writes) = terminal_with(MapEnv::default(), Some((80, 24)));
    terminal.set_title("pi - work\x07\x1b]52;c;Zm9v\x07\u{9b}2J - repo");
    assert_eq!(*writes.lock().unwrap(), vec!["\x1b]0;pi - work ]52;c;Zm9v 2J - repo\x07".to_string()]);
}

#[test]
fn falls_back_to_columns_and_lines_before_default_dimensions() {
    let (terminal, _) = terminal_with(MapEnv::new([("COLUMNS", "123"), ("LINES", "45")]), None);

    assert_eq!(terminal.columns(), 123);
    assert_eq!(terminal.rows(), 45);

    let (terminal, _) = terminal_with(MapEnv::default(), None);
    assert_eq!((terminal.columns(), terminal.rows()), (80, 24));
}
