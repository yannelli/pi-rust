//! Port of upstream `test/terminal.test.ts`, "ProcessTerminal Kitty keyboard protocol negotiation".
//! Upstream swaps `process.stdout.write` and `process.stdin.on`; here the backend `write` hook
//! records writes and `feed_input` / `poll_input_timers` stand in for stdin data and mock timers.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pi_tui::env::{MapEnv, global_state_lock};
use pi_tui::keys::set_kitty_protocol_active;
use pi_tui::process_terminal::{ProcessTerminal, ProcessTerminalBackend};
use pi_tui::program_status::{ProgramState, ProgramStatus};
use pi_tui::terminal::Terminal;

struct Harness {
    terminal: ProcessTerminal,
    writes: Arc<Mutex<Vec<String>>>,
    input: Arc<Mutex<Option<String>>>,
    now: Instant,
    cleaned: bool,
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        Self::with_env(MapEnv::default())
    }

    fn with_env(env: MapEnv) -> Self {
        let guard = global_state_lock();
        set_kitty_protocol_active(false);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let sink = writes.clone();
        let terminal = ProcessTerminal::with_backend(ProcessTerminalBackend {
            env: Arc::new(env),
            write: Box::new(move |data: &str| sink.lock().unwrap().push(data.to_string())),
            size: Box::new(|| None),
            real_io: false,
        });
        let mut harness = Self {
            terminal,
            writes,
            input: Arc::new(Mutex::new(None)),
            now: Instant::now(),
            cleaned: false,
            _guard: guard,
        };
        harness.query_and_enable_kitty_protocol();
        harness
    }

    /// Upstream `queryAndEnableKittyProtocol()` with the recording input handler.
    fn query_and_enable_kitty_protocol(&mut self) {
        let input = self.input.clone();
        self.terminal.begin_input(move |data| *input.lock().unwrap() = Some(data));
    }

    fn send(&self, data: &str) {
        self.terminal.feed_input(data, self.now);
    }

    /// Upstream `mock.timers.tick(ms)`.
    fn tick(&mut self, ms: u64) {
        self.now += Duration::from_millis(ms);
        self.terminal.poll_input_timers(self.now);
    }

    fn input(&self) -> Option<String> {
        self.input.lock().unwrap().clone()
    }

    fn writes(&self) -> Vec<String> {
        self.writes.lock().unwrap().clone()
    }

    fn count(&self, sequence: &str) -> usize {
        self.writes().iter().filter(|write| *write == sequence).count()
    }

    fn includes(&self, sequence: &str) -> bool {
        self.count(sequence) > 0
    }

    fn last_write(&self) -> Option<String> {
        self.writes().last().cloned()
    }

    fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        self.terminal.stop();
        set_kitty_protocol_active(false);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[test]
fn queries_kitty_mode_before_enabling_modify_other_keys_fallback() {
    let harness = Harness::new();
    assert_eq!(harness.writes()[0], "\x1b[>7u\x1b[?u\x1b]7501;?\x1b\\\x1b[c");
    assert!(!harness.includes("\x1b[>4;2m"));
    assert!(!harness.terminal.kitty_protocol_active());
}

#[test]
fn activates_kitty_mode_for_non_zero_negotiated_flags() {
    let mut harness = Harness::new();
    harness.send("\x1b[?7u");

    assert_eq!(harness.input(), None);
    assert!(harness.terminal.kitty_protocol_active());
    assert!(!harness.includes("\x1b[>4;2m"));
    assert!(!harness.includes("\x1b[>4;0m"));

    harness.cleanup();
    assert_eq!(harness.count("\x1b[<u"), 1);
    assert!(!harness.includes("\x1b[>4;0m"));
}

#[test]
fn falls_back_to_modify_other_keys_for_zero_kitty_flags() {
    let mut harness = Harness::new();
    harness.send("\x1b[?0u");

    assert_eq!(harness.input(), None);
    assert!(!harness.terminal.kitty_protocol_active());
    assert_eq!(harness.count("\x1b[>4;2m"), 1);

    harness.cleanup();
    assert_eq!(harness.count("\x1b[>4;0m"), 1);
}

#[test]
fn falls_back_to_modify_other_keys_for_device_attributes_without_kitty_flags() {
    let harness = Harness::new();
    harness.send("\x1b[?62;4;52c");

    assert_eq!(harness.input(), None);
    assert!(!harness.terminal.kitty_protocol_active());
    assert_eq!(harness.count("\x1b[>4;2m"), 1);
}

#[test]
fn forwards_device_attributes_replies_that_answer_other_queries() {
    let harness = Harness::new();
    harness.send("\x1b[?7u");
    harness.send("\x1b[?62;4;52c");
    assert_eq!(harness.input(), None);

    // The TUI's color query uses DA1 as its own sentinel.
    harness.send("\x1b[?62;4;52c");
    assert_eq!(harness.input().as_deref(), Some("\x1b[?62;4;52c"));
}

#[test]
fn forwards_normal_input_while_waiting_for_kitty_response() {
    let harness = Harness::new();
    harness.send("a");

    assert_eq!(harness.input().as_deref(), Some("a"));
    assert!(!harness.terminal.kitty_protocol_active());
}

#[test]
fn tracks_split_kitty_confirmation() {
    let mut harness = Harness::new();
    harness.send("\x1b[?7");
    harness.tick(10);

    assert_eq!(harness.input(), None);

    harness.send("u");

    assert!(harness.terminal.kitty_protocol_active());
    assert!(!harness.includes("\x1b[>4;2m"));
}

#[test]
fn replays_buffered_csi_prefix_input_when_it_is_not_a_kitty_response() {
    let mut harness = Harness::new();
    harness.send("\x1b[");
    harness.tick(50); // StdinBuffer sequence timeout, not the lone-ESC timeout

    assert_eq!(harness.input(), None);

    harness.tick(150);

    assert_eq!(harness.input().as_deref(), Some("\x1b["));
}

// program status (OSC 7501), #10607

const WORKING: &str = "\x1b]7501;state=working:app=pi\x1b\\";
const CLEAR: &str = "\x1b]7501;state=clear\x1b\\";

fn working() -> ProgramStatus {
    ProgramStatus { app: Some("pi".to_string()), ..ProgramStatus::new(ProgramState::Working) }
}

fn with_override(value: &str) -> Harness {
    Harness::with_env(MapEnv::new([("PI_PROGRAM_STATUS", value)]))
}

#[test]
fn reports_the_latest_status_once_the_terminal_answers_the_query_before_da() {
    let mut harness = Harness::new();
    harness.terminal.set_program_status(&working());
    assert!(!harness.includes(WORKING));

    harness.send("\x1b]7501;?\x1b\\");
    assert_eq!(harness.input(), None);
    assert_eq!(harness.count(WORKING), 1);
    harness.send("\x1b[?62;4;52c");

    harness.terminal.set_program_status(&ProgramStatus::new(ProgramState::Done));
    assert_eq!(harness.last_write().as_deref(), Some("\x1b]7501;state=done\x1b\\"));
    harness.cleanup();
}

#[test]
fn reports_nothing_when_da_arrives_first_and_swallows_late_replies() {
    let mut harness = Harness::new();
    harness.send("\x1b[?62;4;52c");
    harness.send("\x1b]7501;?\x07");
    harness.terminal.set_program_status(&working());

    assert_eq!(harness.input(), None);
    assert!(!harness.writes().iter().any(|write| write.starts_with("\x1b]7501;state=")));
    harness.cleanup();
}

#[test]
fn skips_the_query_when_pi_program_status_is_set() {
    {
        let mut harness = with_override("1");
        assert_eq!(harness.writes()[0], "\x1b[>7u\x1b[?u\x1b[c");
        harness.terminal.set_program_status(&working());
        assert_eq!(harness.last_write().as_deref(), Some(WORKING));
    }
    {
        let mut harness = with_override("0");
        assert_eq!(harness.writes()[0], "\x1b[>7u\x1b[?u\x1b[c");
        harness.terminal.set_program_status(&working());
        assert!(!harness.includes(WORKING));
    }
}

#[test]
fn does_not_let_a_da_reply_from_before_a_restart_end_the_new_query() {
    let mut harness = Harness::new();
    harness.terminal.set_program_status(&working());
    harness.terminal.stop();
    harness.query_and_enable_kitty_protocol();

    // The first start's replies arrive late: its DA, then the second start's reply and DA.
    harness.send("\x1b[?62;4;52c");
    harness.send("\x1b]7501;?\x1b\\");
    harness.send("\x1b[?62;4;52c");
    assert_eq!(harness.last_write().as_deref(), Some(WORKING));
    assert_eq!(harness.input(), None);
}

#[test]
fn clears_the_status_on_stop_and_reports_it_again_after_restart() {
    let mut harness = Harness::new();
    harness.terminal.set_program_status(&working());
    harness.send("\x1b]7501;?\x1b\\");
    harness.terminal.stop();
    assert!(harness.includes(CLEAR));

    // Stopped: nothing is written until the restarted terminal confirms support again.
    let writes_before_restart = harness.writes().len();
    harness.terminal.set_program_status(&working());
    assert_eq!(harness.writes().len(), writes_before_restart);

    harness.query_and_enable_kitty_protocol();
    harness.send("\x1b]7501;?\x1b\\");
    assert_eq!(harness.last_write().as_deref(), Some(WORKING));
}
