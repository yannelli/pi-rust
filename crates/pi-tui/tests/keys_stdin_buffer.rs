//! Port of upstream stdin-buffer.test.ts, part 1: characters, escape sequences, partial input,
//! mixed content, and Kitty. Upstream `await wait(ms)` advances a fake clock and fires `on_timeout`.

use std::time::{Duration, Instant};

use pi_tui::env::global_state_lock;
use pi_tui::keys::{matches_key, set_kitty_protocol_active};
use pi_tui::stdin_buffer::{StdinBuffer, StdinBufferEvent, StdinBufferOptions};

struct Harness {
    buffer: StdinBuffer,
    now: Instant,
    data: Vec<String>,
}

impl Harness {
    fn with_options(options: StdinBufferOptions) -> Self {
        Self { buffer: StdinBuffer::new(options), now: Instant::now(), data: Vec::new() }
    }

    /// Upstream `beforeEach`: `new StdinBuffer({ timeout: 10 })`.
    fn new() -> Self {
        Self::with_options(StdinBufferOptions { timeout: Duration::from_millis(10), ..Default::default() })
    }

    fn record(&mut self, events: Vec<StdinBufferEvent>) {
        for event in events {
            match event {
                StdinBufferEvent::Data(sequence) => self.data.push(sequence),
                StdinBufferEvent::Paste(content) => panic!("unexpected paste {content:?}"),
            }
        }
    }

    fn process(&mut self, input: &str) {
        let events = self.buffer.process(input, self.now);
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
fn passes_through_regular_characters_immediately() {
    assert_splits("a", &["a"]);
}

#[test]
fn passes_through_multiple_regular_characters() {
    assert_splits("abc", &["a", "b", "c"]);
}

#[test]
fn handles_unicode_characters() {
    assert_splits("hello \u{4e16}\u{754c}", &["h", "e", "l", "l", "o", " ", "\u{4e16}", "\u{754c}"]);
}

#[test]
fn passes_through_complete_mouse_sgr_sequences() {
    assert_splits("\x1b[<35;20;5m", &["\x1b[<35;20;5m"]);
}

#[test]
fn passes_through_complete_arrow_key_sequences() {
    assert_splits("\x1b[A", &["\x1b[A"]);
}

#[test]
fn passes_through_complete_function_key_sequences() {
    assert_splits("\x1b[11~", &["\x1b[11~"]);
}

#[test]
fn passes_through_meta_key_sequences() {
    assert_splits("\x1ba", &["\x1ba"]);
}

#[test]
fn passes_through_ss3_sequences() {
    assert_splits("\x1bOA", &["\x1bOA"]);
}

#[test]
fn buffers_incomplete_mouse_sgr_sequence() {
    let mut h = Harness::new();
    h.process("\x1b");
    assert!(h.data.is_empty());
    assert_eq!(h.buffer.get_buffer(), "\x1b");
    h.process("[<35");
    assert!(h.data.is_empty());
    assert_eq!(h.buffer.get_buffer(), "\x1b[<35");
    h.process(";20;5m");
    assert_eq!(h.data, ["\x1b[<35;20;5m"]);
    assert_eq!(h.buffer.get_buffer(), "");
}

#[test]
fn buffers_incomplete_csi_sequence() {
    let mut h = Harness::new();
    h.process("\x1b[");
    assert!(h.data.is_empty());
    h.process("1;");
    assert!(h.data.is_empty());
    h.process("5H");
    assert_eq!(h.data, ["\x1b[1;5H"]);
}

#[test]
fn buffers_split_across_many_chunks() {
    let mut h = Harness::new();
    for chunk in ["\x1b", "[", "<", "3", "5", ";", "2", "0", ";", "5", "m"] {
        h.process(chunk);
    }
    assert_eq!(h.data, ["\x1b[<35;20;5m"]);
}

#[test]
fn flushes_incomplete_sequence_after_timeout() {
    let mut h = Harness::new();
    h.process("\x1b[<35");
    assert!(h.data.is_empty());
    h.wait(15);
    assert_eq!(h.data, ["\x1b[<35"]);
}

#[test]
fn flushes_lone_esc_as_escape_when_cr_arrives_after_timeout() {
    let _lock = global_state_lock();
    set_kitty_protocol_active(false);
    let mut h = Harness::new();
    h.process("\x1b");
    h.wait(20);
    h.process("\r");
    assert_eq!(h.data, ["\x1b", "\r"]);
    assert!(matches_key(&h.data[0], "escape"));
}

#[test]
fn merges_esc_cr_split_across_chunks_within_larger_escape_timeout() {
    let _lock = global_state_lock();
    set_kitty_protocol_active(false);
    let mut h =
        Harness::with_options(StdinBufferOptions { escape_timeout: Duration::from_millis(100), ..Default::default() });
    h.process("\x1b");
    h.wait(20);
    h.process("\r");
    assert_eq!(h.data, ["\x1b\r"]);
    assert!(matches_key(&h.data[0], "alt+enter"));
}

#[test]
fn does_not_apply_sequence_timeout_to_lone_esc() {
    let _lock = global_state_lock();
    set_kitty_protocol_active(false);
    let mut h = Harness::with_options(StdinBufferOptions { timeout: Duration::from_millis(100), ..Default::default() });
    h.process("\x1b");
    h.wait(20);
    h.process("\r");
    assert_eq!(h.data, ["\x1b", "\r"]);
    assert!(matches_key(&h.data[0], "escape"));
}

#[test]
fn keeps_fragmented_mouse_sequences_buffered_across_delayed_chunks_by_default() {
    let mut h = Harness::with_options(StdinBufferOptions::default());
    h.process("\x1b[");
    h.wait(20);
    assert!(h.data.is_empty());
    h.process("<65;48;39M");
    assert_eq!(h.data, ["\x1b[<65;48;39M"]);
    h.buffer.destroy();
}

#[test]
fn handles_characters_followed_by_escape_sequence() {
    assert_splits("abc\x1b[A", &["a", "b", "c", "\x1b[A"]);
}

#[test]
fn handles_escape_sequence_followed_by_characters() {
    assert_splits("\x1b[Aabc", &["\x1b[A", "a", "b", "c"]);
}

#[test]
fn handles_multiple_complete_sequences() {
    assert_splits("\x1b[A\x1b[B\x1b[C", &["\x1b[A", "\x1b[B", "\x1b[C"]);
}

#[test]
fn handles_partial_sequence_with_preceding_characters() {
    let mut h = Harness::new();
    h.process("abc\x1b[<35");
    assert_eq!(h.data, ["a", "b", "c"]);
    assert_eq!(h.buffer.get_buffer(), "\x1b[<35");
    h.process(";20;5m");
    assert_eq!(h.data, ["a", "b", "c", "\x1b[<35;20;5m"]);
}

#[test]
fn handles_kitty_csi_u_press_events() {
    assert_splits("\x1b[97u", &["\x1b[97u"]);
}

#[test]
fn handles_kitty_csi_u_release_events() {
    assert_splits("\x1b[97;1:3u", &["\x1b[97;1:3u"]);
}

#[test]
fn handles_batched_kitty_press_and_release() {
    assert_splits("\x1b[97u\x1b[97;1:3u", &["\x1b[97u", "\x1b[97;1:3u"]);
}

#[test]
fn handles_multiple_batched_kitty_events() {
    assert_splits(
        "\x1b[97u\x1b[97;1:3u\x1b[98u\x1b[98;1:3u",
        &["\x1b[97u", "\x1b[97;1:3u", "\x1b[98u", "\x1b[98;1:3u"],
    );
}

#[test]
fn handles_kitty_arrow_keys_with_event_type() {
    assert_splits("\x1b[1;1:1A", &["\x1b[1;1:1A"]);
}

#[test]
fn handles_kitty_functional_keys_with_event_type() {
    assert_splits("\x1b[3;1:3~", &["\x1b[3;1:3~"]);
}

#[test]
fn splits_esc_esc_csi_into_lone_esc_and_csi_for_wezterm() {
    assert_splits("\x1b\x1b[27;129:3u", &["\x1b", "\x1b[27;129:3u"]);
}

#[test]
fn splits_esc_esc_csi_without_modifier() {
    assert_splits("\x1b\x1b[27;1:3u", &["\x1b", "\x1b[27;1:3u"]);
}

#[test]
fn keeps_esc_esc_as_one_sequence_when_no_new_escape_follows() {
    assert_splits("\x1b\x1b", &["\x1b\x1b"]);
}

#[test]
fn handles_plain_characters_mixed_with_kitty_sequences() {
    assert_splits("a\x1b[97;1:3u", &["a", "\x1b[97;1:3u"]);
}

#[test]
fn drops_raw_duplicate_after_matching_kitty_printable_sequence() {
    assert_splits("\x1b[224u\u{e0}", &["\x1b[224u"]);
}

#[test]
fn drops_raw_duplicate_after_matching_kitty_printable_sequence_across_chunks() {
    let mut h = Harness::new();
    h.process("\x1b[64u");
    h.process("@");
    assert_eq!(h.data, ["\x1b[64u"]);
}

#[test]
fn keeps_non_matching_plain_character_after_kitty_printable_sequence() {
    assert_splits("\x1b[97ub", &["\x1b[97u", "b"]);
}

#[test]
fn keeps_raw_character_after_modified_kitty_printable_sequence() {
    assert_splits("\x1b[64;3u@", &["\x1b[64;3u", "@"]);
}

#[test]
fn handles_rapid_typing_with_kitty_protocol() {
    let input = "\x1b[104u\x1b[104;1:3u\x1b[105u\x1b[105;1:3u";
    assert_splits(input, &["\x1b[104u", "\x1b[104;1:3u", "\x1b[105u", "\x1b[105;1:3u"]);
}
