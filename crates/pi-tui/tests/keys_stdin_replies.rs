//! Port of upstream stdin-buffer.test.ts, "Terminal Reply Strings". Upstream mocks `setTimeout`;
//! here a fake clock advances and fires `on_timeout`.

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

    /// Upstream `new StdinBuffer()`.
    fn default_timeouts() -> Self {
        Self::with_options(StdinBufferOptions::default())
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

    fn tick(&mut self, ms: u64) {
        self.now += Duration::from_millis(ms);
        let events = self.buffer.on_timeout(self.now);
        self.record(events);
    }
}

/// (header, reply) pairs. The first reply is Ghostty's Ayu Light palette color 1.
const REPLIES: [(&str, &str); 7] = [
    ("\x1b]4;1;", "\x1b]4;1;rgb:eaea/6c6c/6d6d\x07"),
    ("\x1b]10;", "\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
    ("\x1b]11;", "\x1b]11;rgb:0000/0000/0000\x07"),
    ("\x1b]4;1;", "\x1b]4;1;rgb:6464/6464/4d4d\x07"),
    ("\x1bP>|", "\x1bP>|Ghostty 1.3.1\x1b\\"),
    ("\x1b_Gi=1;", "\x1b_Gi=1;OK\x1b\\"),
    ("\x1b]7501;?", "\x1b]7501;?\x1b\\"),
];

#[test]
fn keeps_delayed_replies_intact_at_every_split_after_their_header() {
    let _lock = global_state_lock();
    set_kitty_protocol_active(false);
    for (header, reply) in REPLIES {
        for split in header.len()..reply.len() {
            let mut h = Harness::default_timeouts();
            h.process(&reply[..split]);
            h.tick(5000);
            h.process(&format!("{}x", &reply[split..]));
            assert!(!h.data.iter().any(|sequence| matches_key(sequence, "ctrl+g")), "{reply:?} at {split}");
            assert_eq!(h.data, [reply, "x"], "{reply:?} at {split}");
        }
    }
}

#[test]
fn keeps_an_osc_reply_buffered_across_multiple_expired_sequence_timeouts() {
    let mut h = Harness::new();
    h.process("\x1b]4;1;rgb:eaea/6c");
    h.tick(100);
    assert!(h.data.is_empty());
    h.process("6c/6d6d");
    h.tick(100);
    assert!(h.data.is_empty());
    h.process("\x07");
    assert_eq!(h.data, [REPLIES[0].1]);
}

#[test]
fn still_times_out_bare_string_introducers_as_alt_keys() {
    let mut h = Harness::new();
    for prefix in ["\x1b]", "\x1bP", "\x1b_"] {
        h.process(prefix);
        h.tick(10);
    }
    assert_eq!(h.data, ["\x1b]", "\x1bP", "\x1b_"]);
}

#[test]
fn recovers_batched_legacy_alt_input_within_the_sequence_timeout() {
    for ambiguous in [
        "\x1b]a",
        "\x1b]1",
        "\x1b]10",
        "\x1b]0;title",
        "\x1b]4;",
        "\x1b]4;a;",
        "\x1bPa",
        "\x1bP>",
        "\x1b_a",
        "\x1b_G",
        "\x1b_Gi=1",
    ] {
        let mut h = Harness::default_timeouts();
        h.process(ambiguous);
        h.tick(20);
        h.process("x\r\x03\x04");
        h.tick(49);
        assert!(h.data.is_empty(), "{ambiguous:?}");
        h.tick(1);
        let mut expected = vec![ambiguous[..2].to_string()];
        expected.extend(ambiguous[2..].chars().map(String::from));
        expected.extend(["x", "\r", "\x03", "\x04"].map(String::from));
        assert_eq!(h.data, expected, "{ambiguous:?}");
        assert_eq!(h.buffer.get_buffer(), "");

        h.process("y\r\x03\x04");
        assert_eq!(h.data[h.data.len() - 4..], ["y", "\r", "\x03", "\x04"]);
    }
}

#[test]
fn recognizes_a_reply_header_assembled_within_the_ambiguous_input_timeout() {
    let mut h = Harness::new();
    h.process("\x1b]1");
    h.tick(5);
    h.process("0;");
    h.tick(5000);
    assert!(h.data.is_empty());
    h.process("rgb:ffff/ffff/ffff\x1b\\");
    assert_eq!(h.data, [REPLIES[1].1]);
}

#[test]
fn recovers_from_an_unterminated_reply_when_a_new_escape_sequence_arrives() {
    let mut h = Harness::new();
    h.process("\x1b]4;1;rgb:eaea/6c");
    h.tick(100);
    h.process("\x1b");
    h.tick(100);
    h.process("[Ahello");
    assert_eq!(h.data, ["\x1b[A", "h", "e", "l", "l", "o"]);
}

#[test]
fn discards_a_canceled_reply_without_swallowing_subsequent_input() {
    let mut h = Harness::new();
    for cancel in ["\x18", "\x1a"] {
        h.process(&format!("\x1b]11;rgb:0000{cancel}x"));
    }
    assert_eq!(h.data, ["x", "x"]);
}
