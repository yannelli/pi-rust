//! Splits batched or fragmented stdin input into complete key and escape sequences.
//! Upstream emits events and runs timers; here `process` returns events and the caller polls
//! [`StdinBuffer::deadline`] and calls [`StdinBuffer::on_timeout`]. Based on OpenTUI (MIT).

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

const ESC: &str = "\x1b";
const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";

/// Color and program-status OSC replies, XTVersion, and Kitty graphics replies. Until their
/// header is complete, the input may be batched legacy Alt keystrokes.
static TERMINAL_REPLY_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\x1b(?:\](?:1[01];|4;[0-9]{1,3};|7501;\?)|P>\||_G(?:[a-z]=[0-9]+,)*[a-z]=[0-9]+;)")
        .expect("valid regex")
});

/// An SGR mouse report cut short: `ESC [ <` and up to three numbers. Bounded so typed text
/// cannot grow it.
static SGR_MOUSE_HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x1b\[<[0-9]{0,4}(?:;[0-9]{0,5}){0,2}$").expect("valid regex"));

/// Whether `data` starts with the rest of a timed-out SGR mouse `head`: digits and separators up
/// to the final `M` or `m`, so that the joined text is still a valid report or report prefix.
fn continues_sgr_mouse_head(head: &str, data: &str) -> bool {
    let end = data.find(['M', 'm']).map_or(data.len(), |index| index + 1);
    let continuation = &data[..end];
    let numbers = continuation.trim_end_matches(['M', 'm']);
    if continuation.is_empty() || !numbers.bytes().all(|byte| byte.is_ascii_digit() || byte == b';') {
        return false;
    }
    let joined = format!("{head}{continuation}");
    SGR_MOUSE_HEAD.is_match(&joined) || is_sgr_mouse_payload(&joined[2..])
}

fn is_control_string(data: &str) -> bool {
    data.starts_with("\x1b]") || data.starts_with("\x1bP") || data.starts_with("\x1b_")
}

/// Timeouts for flushing incomplete input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdinBufferOptions {
    /// How long to wait for the rest of an incomplete keyboard or mouse sequence (default 50ms).
    /// Strings with a recognized terminal reply header wait for their terminator instead, so
    /// delayed replies cannot become keypresses. Ambiguous legacy Alt-key input still times out.
    pub timeout: Duration,
    /// How long to wait after a lone ESC before it counts as Escape (default 10ms). Raise it for
    /// high-latency Alt+key input over SSH.
    pub escape_timeout: Duration,
}

impl Default for StdinBufferOptions {
    fn default() -> Self {
        Self { timeout: Duration::from_millis(50), escape_timeout: Duration::from_millis(10) }
    }
}

/// Output of the buffer: one complete input sequence, or the content of a bracketed paste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdinBufferEvent {
    /// One complete sequence (a character, an escape sequence, or a flushed fragment).
    Data(String),
    /// Text between bracketed paste markers, without the markers.
    Paste(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequenceStatus {
    Complete,
    Incomplete,
    NotEscape,
}

fn char_count_at_least(text: &str, count: usize) -> bool {
    count == 0 || text.chars().nth(count - 1).is_some()
}

fn is_complete_sequence(data: &str) -> SequenceStatus {
    let Some(after_esc) = data.strip_prefix(ESC) else {
        return SequenceStatus::NotEscape;
    };
    if after_esc.is_empty() {
        return SequenceStatus::Incomplete;
    }
    let complete_if = |done: bool| if done { SequenceStatus::Complete } else { SequenceStatus::Incomplete };
    if after_esc.starts_with("[M") {
        // Old-style mouse: ESC [ M plus 3 bytes.
        return complete_if(char_count_at_least(data, 6));
    }
    match after_esc.as_bytes()[0] {
        b'[' => is_complete_csi_sequence(data),
        // OSC ends with ST (ESC \) or BEL.
        b']' => complete_if(data.ends_with("\x1b\\") || data.ends_with('\x07')),
        // DCS (XTVersion replies) and APC (Kitty graphics replies) end with ST.
        b'P' | b'_' => complete_if(data.ends_with("\x1b\\")),
        // SS3 is ESC O plus one character.
        b'O' => complete_if(char_count_at_least(after_esc, 2)),
        // Meta keys (ESC plus one character) and unknown sequences count as complete.
        _ => SequenceStatus::Complete,
    }
}

fn is_sgr_mouse_payload(payload: &str) -> bool {
    let Some(body) = payload.strip_prefix('<').and_then(|p| p.strip_suffix(['M', 'm'])) else {
        return false;
    };
    let parts: Vec<&str> = body.split(';').collect();
    parts.len() == 3 && parts.iter().all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// CSI ends with a final byte in 0x40..=0x7E; SGR mouse payloads (`<b;x;yM`) must be whole.
fn is_complete_csi_sequence(data: &str) -> SequenceStatus {
    if !char_count_at_least(data, 3) {
        return SequenceStatus::Incomplete;
    }
    let payload = &data[2..];
    let last = payload.chars().next_back().map_or(0, u32::from);
    if !(0x40..=0x7e).contains(&last) {
        return SequenceStatus::Incomplete;
    }
    if payload.starts_with('<') && !is_sgr_mouse_payload(payload) {
        return SequenceStatus::Incomplete;
    }
    SequenceStatus::Complete
}

static UNMODIFIED_KITTY_PRINTABLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x1b\[([0-9]+)(?::[0-9]*)?(?::[0-9]+)?u$").expect("valid regex"));

fn parse_unmodified_kitty_printable_codepoint(sequence: &str) -> Option<u32> {
    let captures = UNMODIFIED_KITTY_PRINTABLE.captures(sequence)?;
    captures[1].parse::<u32>().ok().filter(|codepoint| *codepoint >= 32)
}

/// Splits `buffer` into complete sequences plus the incomplete tail.
fn extract_complete_sequences(buffer: &str) -> (Vec<String>, String) {
    let mut sequences = Vec::new();
    let mut pos = 0;
    while pos < buffer.len() {
        let rest = &buffer[pos..];
        if !rest.starts_with(ESC) {
            let ch = rest.chars().next().expect("text left to split");
            sequences.push(ch.to_string());
            pos += ch.len_utf8();
            continue;
        }
        let terminal_reply = is_control_string(rest) && TERMINAL_REPLY_HEADER.is_match(rest);
        let ends = rest.char_indices().map(|(index, ch)| index + ch.len_utf8());
        let mut consumed = None;
        for (count, end) in ends.enumerate() {
            if terminal_reply && count >= 2 {
                match rest[..end].chars().next_back() {
                    Some('\x18' | '\x1a') => {
                        // CAN and SUB cancel the string, not subsequent keyboard input.
                        consumed = Some(end);
                        break;
                    }
                    Some('\x1b') if end < rest.len() && !rest[end..].starts_with('\\') => {
                        // A new escape sequence aborts an unterminated reply. A trailing ESC stays
                        // buffered until it can be told apart from ST.
                        consumed = Some(end - 1);
                        break;
                    }
                    _ => {}
                }
            }
            let candidate = &rest[..end];
            match is_complete_sequence(candidate) {
                SequenceStatus::Incomplete => continue,
                SequenceStatus::Complete if candidate == "\x1b\x1b" && starts_new_escape(&rest[end..]) => {
                    // WezTerm sends the Escape press as raw ESC and the release as CSI u, so
                    // ESC ESC [ is a lone Escape followed by a new sequence.
                    sequences.push(ESC.to_string());
                    consumed = Some(1);
                }
                SequenceStatus::Complete | SequenceStatus::NotEscape => {
                    sequences.push(candidate.to_string());
                    consumed = Some(end);
                }
            }
            break;
        }
        match consumed {
            Some(length) => pos += length,
            None => return (sequences, rest.to_string()),
        }
    }
    (sequences, String::new())
}

fn starts_new_escape(rest: &str) -> bool {
    matches!(rest.as_bytes().first(), Some(b'[' | b']' | b'O' | b'P' | b'_'))
}

/// Buffers stdin input and returns complete sequences, holding partial escape sequences
/// until they complete or their deadline passes.
#[derive(Debug, Clone)]
pub struct StdinBuffer {
    buffer: String,
    deadline: Option<Instant>,
    timeout: Duration,
    escape_timeout: Duration,
    paste_mode: bool,
    paste_buffer: String,
    pending_kitty_printable_codepoint: Option<u32>,
    /// An SGR mouse head that the timeout flushed. A busy reader can split a report, and its tail
    /// must not reach the editor as text, so input that completes the head rejoins it.
    flushed_mouse_head: Option<String>,
}

impl Default for StdinBuffer {
    fn default() -> Self {
        Self::new(StdinBufferOptions::default())
    }
}

impl StdinBuffer {
    /// Creates an empty buffer with the given timeouts.
    pub fn new(options: StdinBufferOptions) -> Self {
        Self {
            buffer: String::new(),
            deadline: None,
            timeout: options.timeout,
            escape_timeout: options.escape_timeout,
            paste_mode: false,
            paste_buffer: String::new(),
            pending_kitty_printable_codepoint: None,
            flushed_mouse_head: None,
        }
    }

    /// Feeds input received at `now` and returns the complete sequences and pastes.
    /// If an earlier deadline already passed, the pending fragment is flushed first, as the
    /// upstream timer would have fired before this input arrived.
    pub fn process(&mut self, data: &str, now: Instant) -> Vec<StdinBufferEvent> {
        let mut events = self.on_timeout(now);
        self.process_str(data, now, &mut events);
        events
    }

    /// Feeds raw bytes. A single byte above 127 becomes ESC plus `byte - 128` (legacy Meta);
    /// other input is decoded as UTF-8 with replacement characters.
    pub fn process_bytes(&mut self, data: &[u8], now: Instant) -> Vec<StdinBufferEvent> {
        match data {
            [byte] if *byte > 127 => {
                let text = format!("{ESC}{}", char::from(byte - 128));
                self.process(&text, now)
            }
            _ => self.process(&String::from_utf8_lossy(data), now),
        }
    }

    /// When the pending incomplete sequence must be flushed, if there is one.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Flushes the pending fragment as data when the deadline has passed; otherwise returns nothing.
    pub fn on_timeout(&mut self, now: Instant) -> Vec<StdinBufferEvent> {
        let mut events = Vec::new();
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            for sequence in self.flush() {
                match sequence.char_indices().nth(2) {
                    Some((tail, _)) if is_control_string(&sequence) => {
                        // Recover the Alt key and reparse its tail so batched text, Enter, and
                        // control keys are not swallowed as one event.
                        self.emit_data(sequence[..tail].to_string(), &mut events);
                        self.process_str(&sequence[tail..], now, &mut events);
                    }
                    _ => {
                        if SGR_MOUSE_HEAD.is_match(&sequence) {
                            self.flushed_mouse_head = Some(sequence.clone());
                        }
                        self.emit_data(sequence, &mut events);
                    }
                }
            }
        }
        events
    }

    fn process_str(&mut self, data: &str, now: Instant, events: &mut Vec<StdinBufferEvent>) {
        self.deadline = None;

        if data.is_empty() && self.buffer.is_empty() {
            self.emit_data(String::new(), events);
            return;
        }

        if let Some(head) = self.flushed_mouse_head.take()
            && self.buffer.is_empty()
            && !self.paste_mode
            && continues_sgr_mouse_head(&head, data)
        {
            self.buffer = head;
        }
        self.buffer.push_str(data);

        if self.paste_mode {
            let buffered = std::mem::take(&mut self.buffer);
            self.paste_buffer.push_str(&buffered);
            self.finish_paste_if_complete(now, events);
            return;
        }

        if let Some(start) = self.buffer.find(BRACKETED_PASTE_START) {
            if start > 0 {
                // Complete sequences before the paste are emitted; an incomplete tail is dropped.
                let (sequences, _) = extract_complete_sequences(&self.buffer[..start]);
                for sequence in sequences {
                    self.emit_data(sequence, events);
                }
            }
            self.pending_kitty_printable_codepoint = None;
            self.paste_buffer = self.buffer[start + BRACKETED_PASTE_START.len()..].to_string();
            self.buffer.clear();
            self.paste_mode = true;
            self.finish_paste_if_complete(now, events);
            return;
        }

        let (sequences, tail) = extract_complete_sequences(&self.buffer);
        self.buffer = tail;
        for sequence in sequences {
            self.emit_data(sequence, events);
        }

        // Only a recognizable reply header establishes a delimited string. Flushing it would turn
        // a delayed tail into text, and an OSC's BEL terminator into Ctrl+G. Other input,
        // including Alt+] followed by text, keeps its bounded recovery.
        if !self.buffer.is_empty() && !TERMINAL_REPLY_HEADER.is_match(&self.buffer) {
            let wait = if self.buffer == ESC { self.escape_timeout } else { self.timeout };
            self.deadline = Some(now + wait);
        }
    }

    fn finish_paste_if_complete(&mut self, now: Instant, events: &mut Vec<StdinBufferEvent>) {
        let Some(end) = self.paste_buffer.find(BRACKETED_PASTE_END) else {
            return;
        };
        let pasted = self.paste_buffer[..end].to_string();
        let after = self.paste_buffer[end + BRACKETED_PASTE_END.len()..].to_string();
        self.paste_mode = false;
        self.paste_buffer.clear();
        self.pending_kitty_printable_codepoint = None;
        events.push(StdinBufferEvent::Paste(pasted));
        if !after.is_empty() {
            self.process_str(&after, now, events);
        }
    }

    /// Kitty can report a printable key as CSI u and then also send the raw character; the
    /// raw duplicate that directly follows an unmodified CSI u for the same codepoint is dropped.
    fn emit_data(&mut self, sequence: String, events: &mut Vec<StdinBufferEvent>) {
        let mut chars = sequence.chars();
        let single = match (chars.next(), chars.next()) {
            (Some(ch), None) => Some(u32::from(ch)),
            _ => None,
        };
        if single.is_some() && single == self.pending_kitty_printable_codepoint {
            self.pending_kitty_printable_codepoint = None;
            return;
        }
        self.pending_kitty_printable_codepoint = parse_unmodified_kitty_printable_codepoint(&sequence);
        events.push(StdinBufferEvent::Data(sequence));
    }

    /// Returns the buffered incomplete input as one sequence and clears it, without emitting.
    pub fn flush(&mut self) -> Vec<String> {
        self.deadline = None;
        if self.buffer.is_empty() {
            return Vec::new();
        }
        self.pending_kitty_printable_codepoint = None;
        vec![std::mem::take(&mut self.buffer)]
    }

    /// Drops buffered input, any partial paste, and the pending deadline.
    pub fn clear(&mut self) {
        self.deadline = None;
        self.flushed_mouse_head = None;
        self.buffer.clear();
        self.paste_mode = false;
        self.paste_buffer.clear();
        self.pending_kitty_printable_codepoint = None;
    }

    /// The buffered incomplete input (excludes partial paste content).
    pub fn get_buffer(&self) -> &str {
        &self.buffer
    }

    /// Same as [`StdinBuffer::clear`]; kept for parity with upstream.
    pub fn destroy(&mut self) {
        self.clear();
    }
}
