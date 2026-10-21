//! Port of upstream stdin-buffer.test.ts, part 2: mouse, edge cases, flush, clear, bracketed
//! paste, and destroy, plus checks for byte input and deadlines.

use std::time::{Duration, Instant};

use pi_tui::stdin_buffer::{StdinBuffer, StdinBufferEvent, StdinBufferOptions};

struct Harness {
    buffer: StdinBuffer,
    now: Instant,
    data: Vec<String>,
    pastes: Vec<String>,
}

impl Harness {
    fn with_options(options: StdinBufferOptions) -> Self {
        Self { buffer: StdinBuffer::new(options), now: Instant::now(), data: Vec::new(), pastes: Vec::new() }
    }

    /// Upstream `beforeEach`: `new StdinBuffer({ timeout: 10 })`.
    fn new() -> Self {
        Self::with_options(StdinBufferOptions { timeout: Duration::from_millis(10), ..Default::default() })
    }

    fn record(&mut self, events: Vec<StdinBufferEvent>) {
        for event in events {
            match event {
                StdinBufferEvent::Data(sequence) => self.data.push(sequence),
                StdinBufferEvent::Paste(content) => self.pastes.push(content),
            }
        }
    }

    fn process(&mut self, input: &str) {
        let events = self.buffer.process(input, self.now);
        self.record(events);
    }

    fn process_bytes(&mut self, input: &[u8]) {
        let events = self.buffer.process_bytes(input, self.now);
        self.record(events);
    }

    fn wait(&mut self, ms: u64) {
        self.now += Duration::from_millis(ms);
        let events = self.buffer.on_timeout(self.now);
        self.record(events);
    }
}

fn assert_splits(input: &str, expected: &[&str]) {
    let mut harness = Harness::new();
    harness.process(input);
    assert_eq!(harness.data, expected, "input {input:?}");
}

#[test]
fn handles_mouse_press_event() {
    assert_splits("\x1b[<0;10;5M", &["\x1b[<0;10;5M"]);
}

#[test]
fn handles_mouse_release_event() {
    assert_splits("\x1b[<0;10;5m", &["\x1b[<0;10;5m"]);
}

#[test]
fn handles_mouse_move_event() {
    assert_splits("\x1b[<35;20;5m", &["\x1b[<35;20;5m"]);
}

#[test]
fn handles_split_mouse_events() {
    let mut h = Harness::new();
    for chunk in ["\x1b[<3", "5;1", "5;", "10m"] {
        h.process(chunk);
    }
    assert_eq!(h.data, ["\x1b[<35;15;10m"]);
}

#[test]
fn handles_multiple_mouse_events() {
    assert_splits("\x1b[<35;1;1m\x1b[<35;2;2m\x1b[<35;3;3m", &["\x1b[<35;1;1m", "\x1b[<35;2;2m", "\x1b[<35;3;3m"]);
}

#[test]
fn handles_old_style_mouse_sequence() {
    assert_splits("\x1b[M abc", &["\x1b[M ab", "c"]);
}

#[test]
fn buffers_incomplete_old_style_mouse_sequence() {
    let mut h = Harness::new();
    h.process("\x1b[M");
    assert_eq!(h.buffer.get_buffer(), "\x1b[M");
    h.process(" a");
    assert_eq!(h.buffer.get_buffer(), "\x1b[M a");
    h.process("b");
    assert_eq!(h.data, ["\x1b[M ab"]);
}

#[test]
fn handles_empty_input() {
    assert_splits("", &[""]);
}

#[test]
fn handles_lone_escape_character_with_timeout() {
    let mut h = Harness::new();
    h.process("\x1b");
    assert!(h.data.is_empty());
    h.wait(15);
    assert_eq!(h.data, ["\x1b"]);
}

#[test]
fn flushes_lone_escape_promptly_with_longer_default_sequence_timeout() {
    let mut h = Harness::with_options(StdinBufferOptions::default());
    h.process("\x1b");
    h.wait(20);
    assert_eq!(h.data, ["\x1b"]);
    h.buffer.destroy();
}

#[test]
fn handles_lone_escape_character_with_explicit_flush() {
    let mut h = Harness::new();
    h.process("\x1b");
    assert!(h.data.is_empty());
    assert_eq!(h.buffer.flush(), ["\x1b"]);
}

#[test]
fn handles_buffer_input() {
    let mut h = Harness::new();
    h.process_bytes(b"\x1b[A");
    assert_eq!(h.data, ["\x1b[A"]);
}

#[test]
fn handles_very_long_sequences() {
    let long = format!("\x1b[{}H", "1;".repeat(50));
    assert_splits(&long, &[long.as_str()]);
}

#[test]
fn flushes_incomplete_sequences() {
    let mut h = Harness::new();
    h.process("\x1b[<35");
    assert_eq!(h.buffer.flush(), ["\x1b[<35"]);
    assert_eq!(h.buffer.get_buffer(), "");
}

#[test]
fn flush_returns_empty_when_nothing_buffered() {
    let mut h = Harness::new();
    assert!(h.buffer.flush().is_empty());
}

#[test]
fn emits_flushed_data_via_timeout() {
    let mut h = Harness::new();
    h.process("\x1b[<35");
    assert!(h.data.is_empty());
    h.wait(15);
    assert_eq!(h.data, ["\x1b[<35"]);
}

#[test]
fn clears_buffered_content_without_emitting() {
    let mut h = Harness::new();
    h.process("\x1b[<35");
    assert_eq!(h.buffer.get_buffer(), "\x1b[<35");
    h.buffer.clear();
    assert_eq!(h.buffer.get_buffer(), "");
    assert!(h.data.is_empty());
}

#[test]
fn emits_paste_event_for_complete_bracketed_paste() {
    let mut h = Harness::new();
    h.process("\x1b[200~hello world\x1b[201~");
    assert_eq!(h.pastes, ["hello world"]);
    assert!(h.data.is_empty());
}

#[test]
fn handles_paste_arriving_in_chunks() {
    let mut h = Harness::new();
    h.process("\x1b[200~");
    assert!(h.pastes.is_empty());
    h.process("hello ");
    assert!(h.pastes.is_empty());
    h.process("world\x1b[201~");
    assert_eq!(h.pastes, ["hello world"]);
    assert!(h.data.is_empty());
}

#[test]
fn handles_paste_with_input_before_and_after() {
    let mut h = Harness::new();
    h.process("a");
    h.process("\x1b[200~pasted\x1b[201~");
    h.process("b");
    assert_eq!(h.data, ["a", "b"]);
    assert_eq!(h.pastes, ["pasted"]);
}

#[test]
fn handles_paste_with_newlines() {
    let mut h = Harness::new();
    h.process("\x1b[200~line1\nline2\nline3\x1b[201~");
    assert_eq!(h.pastes, ["line1\nline2\nline3"]);
    assert!(h.data.is_empty());
}

#[test]
fn handles_paste_with_unicode() {
    let mut h = Harness::new();
    h.process("\x1b[200~Hello \u{4e16}\u{754c} \u{1f389}\x1b[201~");
    assert_eq!(h.pastes, ["Hello \u{4e16}\u{754c} \u{1f389}"]);
    assert!(h.data.is_empty());
}

#[test]
fn clears_buffer_on_destroy() {
    let mut h = Harness::new();
    h.process("\x1b[<35");
    assert_eq!(h.buffer.get_buffer(), "\x1b[<35");
    h.buffer.destroy();
    assert_eq!(h.buffer.get_buffer(), "");
}

#[test]
fn clears_pending_timeouts_on_destroy() {
    let mut h = Harness::new();
    h.process("\x1b[<35");
    h.buffer.destroy();
    h.wait(15);
    assert!(h.data.is_empty());
    assert_eq!(h.buffer.deadline(), None);
}

#[test]
fn single_high_byte_becomes_meta_sequence() {
    let mut h = Harness::new();
    h.process_bytes(&[0x80 + b'a']);
    assert_eq!(h.data, ["\x1ba"]);
    h.process_bytes(&[0xff, 0xfe]);
    assert_eq!(h.data, ["\x1ba", "\u{fffd}", "\u{fffd}"]);
}

#[test]
fn deadline_tracks_pending_fragment() {
    let mut h = Harness::new();
    assert_eq!(h.buffer.deadline(), None);
    h.process("\x1b");
    assert_eq!(h.buffer.deadline(), Some(h.now + Duration::from_millis(10)));
    h.process("[");
    assert_eq!(h.buffer.deadline(), Some(h.now + Duration::from_millis(10)));
    h.wait(5);
    assert!(h.data.is_empty(), "nothing flushes before the deadline");
    h.process("A");
    assert_eq!(h.data, ["\x1b[A"]);
    assert_eq!(h.buffer.deadline(), None);
}

#[test]
fn process_flushes_expired_fragment_before_new_input() {
    let mut h = Harness::new();
    h.process("\x1b");
    h.now += Duration::from_millis(20);
    h.process("\r");
    assert_eq!(h.data, ["\x1b", "\r"]);
}

#[test]
fn paste_followed_by_keys_in_one_chunk() {
    let mut h = Harness::new();
    h.process("x\x1b[200~one\x1b[201~\x1b[A\x1b[200~two\x1b[201~y");
    assert_eq!(h.pastes, ["one", "two"]);
    assert_eq!(h.data, ["x", "\x1b[A", "y"]);
}

#[test]
fn rejoins_a_mouse_report_split_by_the_timeout() {
    // Upstream issue 11172: the tail of a split report must not become typed text.
    let mut h = Harness::new();
    h.process("\x1b[<35;4");
    h.wait(40);
    h.process("8;27M");
    assert_eq!(h.data, ["\x1b[<35;4", "\x1b[<35;48;27M"]);
}

#[test]
fn rejoins_a_split_mouse_report_flushed_by_new_input() {
    let mut h = Harness::new();
    h.process("\x1b[<0;12");
    h.now += Duration::from_millis(40);
    h.process(";3m");
    assert_eq!(h.data, ["\x1b[<0;12", "\x1b[<0;12;3m"]);
}

#[test]
fn rejoins_a_mouse_report_tail_that_arrives_in_pieces() {
    let mut h = Harness::new();
    h.process("\x1b[<35;4");
    h.wait(40);
    h.process("8;2");
    assert_eq!(h.data, ["\x1b[<35;4"]);
    h.process("7Mx");
    assert_eq!(h.data, ["\x1b[<35;4", "\x1b[<35;48;27M", "x"]);
}

#[test]
fn types_input_that_does_not_continue_a_flushed_mouse_report() {
    let mut h = Harness::new();
    h.process("\x1b[<35;4");
    h.wait(40);
    h.process("ab");
    h.process("8;27M");
    assert_eq!(h.data, ["\x1b[<35;4", "a", "b", "8", ";", "2", "7", "M"]);
}

#[test]
fn clear_forgets_a_flushed_mouse_report() {
    let mut h = Harness::new();
    h.process("\x1b[<35;4");
    h.wait(40);
    h.buffer.clear();
    h.process("8M");
    assert_eq!(h.data, ["\x1b[<35;4", "8", "M"]);
}
