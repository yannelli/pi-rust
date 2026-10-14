//! Bounded tool output (upstream `harness/output.ts`): sanitizing, slicing to line and byte limits, and the
//! running output buffer of one call. Progress commit pacing lives in [`super::progress`].

use super::tool_registration::Retain;
use crate::env::{ShellOutputSkip, StreamDecoder};
use crate::errors::{DurableError, DurableResult};

/// Retention limits of one tool's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputLimits {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub retain: Retain,
}

/// Kept output and what the limits dropped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoundedOutput {
    pub text: String,
    pub dropped_bytes: u64,
    pub dropped_lines: u64,
}

/// An exact slice of the input within the limits, and what it left out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSlice {
    pub text: String,
    pub bytes: usize,
    pub dropped_bytes: u64,
    pub dropped_lines: u64,
}

const NEWLINE: u8 = b'\n';

fn is_invalid(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{fff9}'..='\u{fffb}')
}

/// Remove control characters that break display and transcripts; tabs and newlines stay.
pub fn sanitize_output(text: &str) -> String {
    if text.chars().any(is_invalid) { text.chars().filter(|c| !is_invalid(*c)).collect() } else { text.to_owned() }
}

/// Bound `text` to whole lines within the limits: the first lines for head, the last lines for tail. A single
/// line longer than `max_bytes` is cut at the byte limit on a character boundary.
pub fn bound_output(text: &str, limits: OutputLimits) -> OutputSlice {
    let bytes = text.as_bytes();
    let (from, to) = match limits.retain {
        Retain::Head => head_range(bytes, limits),
        Retain::Tail => tail_range(bytes, limits),
    };
    let kept = &bytes[from..to];
    OutputSlice {
        text: if kept.len() == bytes.len() { text.to_owned() } else { String::from_utf8_lossy(kept).into_owned() },
        bytes: kept.len(),
        dropped_bytes: (bytes.len() - kept.len()) as u64,
        dropped_lines: (line_count(bytes) - line_count(kept)) as u64,
    }
}

fn head_range(bytes: &[u8], limits: OutputLimits) -> (usize, usize) {
    if limits.max_lines == 0 || limits.max_bytes == 0 {
        return (0, 0);
    }
    let mut end = bytes.len();
    let mut lines = 0;
    for (index, _) in bytes.iter().enumerate().filter(|(_, b)| **b == NEWLINE) {
        lines += 1;
        if lines == limits.max_lines {
            end = index + 1;
            break;
        }
    }
    if end > limits.max_bytes {
        let window = &bytes[..limits.max_bytes];
        end = match window.iter().rposition(|b| *b == NEWLINE) {
            Some(newline) => newline + 1,
            None => character_end(bytes, limits.max_bytes),
        };
    }
    (0, end)
}

fn tail_range(bytes: &[u8], limits: OutputLimits) -> (usize, usize) {
    let length = bytes.len();
    if limits.max_lines == 0 || limits.max_bytes == 0 {
        return (length, length);
    }
    // A trailing newline ends the last line rather than starting another.
    let last = if bytes.last() == Some(&NEWLINE) { length as isize - 2 } else { length as isize - 1 };
    let mut start = 0;
    let mut lines = 1;
    let mut index = if last < 0 { None } else { bytes[..=last as usize].iter().rposition(|b| *b == NEWLINE) };
    while let Some(at) = index {
        if lines == limits.max_lines {
            start = at + 1;
            break;
        }
        lines += 1;
        index = if at == 0 { None } else { bytes[..at].iter().rposition(|b| *b == NEWLINE) };
    }
    if length - start > limits.max_bytes {
        let from = length - limits.max_bytes;
        let search = from.saturating_sub(1);
        let newline = bytes[search..].iter().position(|b| *b == NEWLINE).map(|offset| search + offset);
        start = match newline {
            Some(newline) if newline + 1 < length => newline + 1,
            _ => character_start(bytes, from),
        };
    }
    (start, length)
}

/// The last character boundary at or before `index`.
pub fn character_end(bytes: &[u8], index: usize) -> usize {
    let mut end = index;
    while end > 0 && bytes.get(end).is_some_and(|b| b & 0xc0 == 0x80) {
        end -= 1;
    }
    end
}

fn character_start(bytes: &[u8], index: usize) -> usize {
    let mut start = index;
    while start < bytes.len() && bytes[start] & 0xc0 == 0x80 {
        start += 1;
    }
    start
}

fn line_count(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|b| **b == NEWLINE).count();
    newlines + usize::from(bytes.last() != Some(&NEWLINE))
}

fn count_newlines(text: &str) -> u64 {
    text.bytes().filter(|b| *b == NEWLINE).count() as u64
}

struct Chunk {
    text: String,
    bytes: u64,
    newlines: u64,
}

/// Bounded running output of one tool call. Head retention stops storing once the window is full; tail retention
/// drops stored text the window no longer needs. Counts of the whole stream keep the dropped totals exact.
pub struct OutputBuffer {
    limits: OutputLimits,
    decoder: StreamDecoder,
    started: bool,
    chunks: Vec<Chunk>,
    stored_bytes: u64,
    stored_newlines: u64,
    full: bool,
    total_bytes: u64,
    total_newlines: u64,
    ends_with_newline: bool,
}

/// One chunk of tool output: text, or bytes decoded as UTF-8.
pub enum OutputChunk<'a> {
    Text(&'a str),
    Bytes(&'a [u8]),
}

impl OutputBuffer {
    pub fn new(limits: OutputLimits) -> Self {
        Self {
            limits,
            decoder: StreamDecoder::range(),
            started: false,
            chunks: Vec::new(),
            stored_bytes: 0,
            stored_newlines: 0,
            full: false,
            total_bytes: 0,
            total_newlines: 0,
            ends_with_newline: true,
        }
    }

    /// Bytes held now; bounded by the limits plus one chunk.
    pub fn stored_bytes(&self) -> u64 {
        self.stored_bytes
    }

    /// Accept a chunk; returns whether anything was accepted. `skipped` is output omitted right before the chunk;
    /// only tail retention accepts it.
    pub fn push(&mut self, chunk: OutputChunk<'_>, skipped: Option<ShellOutputSkip>) -> DurableResult<bool> {
        let is_text = matches!(chunk, OutputChunk::Text(_));
        let pending = if is_text || skipped.is_some() { self.decoder.finish() } else { String::new() };
        let mut text = match chunk {
            OutputChunk::Text(text) => text.to_owned(),
            OutputChunk::Bytes(bytes) => self.decoder.decode(bytes),
        };
        let first = !self.started && pending.is_empty() && skipped.is_none();
        if !pending.is_empty() || !text.is_empty() || skipped.is_some() {
            self.started = true;
        }
        if first && !is_text && text.starts_with('\u{feff}') {
            text.remove(0);
        }
        let Some(skipped) = skipped else {
            let mut joined = pending;
            joined.push_str(&text);
            return Ok(self.accept(&joined));
        };
        if self.limits.retain != Retain::Tail {
            return Err(DurableError::error("Skipped output requires tail retention"));
        }
        self.accept(&pending);
        self.skip(skipped);
        self.accept(&text);
        Ok(true)
    }

    fn skip(&mut self, skipped: ShellOutputSkip) {
        if skipped.bytes == 0 {
            return;
        }
        self.total_bytes += skipped.bytes;
        self.total_newlines += skipped.newlines;
        self.ends_with_newline = skipped.ends_with_newline;
        self.chunks.clear();
        self.stored_bytes = 0;
        self.stored_newlines = 0;
    }

    /// Flush an incomplete trailing character as a replacement character; call when the stream ends.
    pub fn end(&mut self) {
        let pending = self.decoder.finish();
        self.accept(&pending);
    }

    fn accept(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let bytes = text.len() as u64;
        let newlines = count_newlines(text);
        self.total_bytes += bytes;
        self.total_newlines += newlines;
        self.ends_with_newline = text.ends_with('\n');
        if self.full {
            return true;
        }
        self.chunks.push(Chunk { text: text.to_owned(), bytes, newlines });
        self.stored_bytes += bytes;
        self.stored_newlines += newlines;
        let (max_bytes, max_lines) = (self.limits.max_bytes as u64, self.limits.max_lines as u64);
        if self.limits.retain == Retain::Head {
            self.full = self.stored_bytes > max_bytes || self.stored_newlines >= max_lines;
            return true;
        }
        let mut drop = 0;
        while self.chunks.len() - drop > 1 {
            let first = &self.chunks[drop];
            let bytes_after = self.stored_bytes - first.bytes;
            let newlines_after = self.stored_newlines - first.newlines;
            if bytes_after <= max_bytes + 1 && newlines_after <= max_lines + 1 {
                break;
            }
            self.stored_bytes = bytes_after;
            self.stored_newlines = newlines_after;
            drop += 1;
        }
        self.chunks.drain(..drop);
        true
    }

    /// Kept, sanitized output and what the limits dropped from the whole stream.
    pub fn snapshot(&mut self) -> BoundedOutput {
        let stored: String = self.chunks.iter().map(|chunk| chunk.text.as_str()).collect();
        let kept = bound_output(&stored, self.limits);
        let stored_lines = lines(self.stored_newlines, stored.is_empty() || stored.ends_with('\n'));
        let kept_lines = stored_lines - kept.dropped_lines;
        if self.limits.retain == Retain::Tail || self.chunks.len() > 1 {
            let text = if self.limits.retain == Retain::Tail { tail_margin(&stored, self.limits) } else { stored };
            let bytes = text.len() as u64;
            let newlines = count_newlines(&text);
            self.chunks = if text.is_empty() { Vec::new() } else { vec![Chunk { text, bytes, newlines }] };
            self.stored_bytes = bytes;
            self.stored_newlines = newlines;
        }
        BoundedOutput {
            text: sanitize_output(&kept.text),
            dropped_bytes: self.total_bytes - kept.bytes as u64,
            dropped_lines: lines(self.total_newlines, self.ends_with_newline) - kept_lines,
        }
    }
}

/// The shortest suffix of `text` with more than `max_bytes` bytes or more than `max_lines` newlines, or all of it.
fn tail_margin(text: &str, limits: OutputLimits) -> String {
    let bytes = text.as_bytes();
    let byte_start =
        if bytes.len() > limits.max_bytes { character_end(bytes, bytes.len() - limits.max_bytes - 1) } else { 0 };
    let mut line_start = 0;
    let mut newlines = 0;
    for (index, _) in bytes.iter().enumerate().rev().filter(|(_, b)| **b == NEWLINE) {
        newlines += 1;
        if newlines > limits.max_lines {
            line_start = index;
            break;
        }
    }
    let start = byte_start.max(line_start);
    if start == 0 { text.to_owned() } else { String::from_utf8_lossy(&bytes[start..]).into_owned() }
}

/// Lines of text with `newlines` newlines; a final unterminated line counts.
fn lines(newlines: u64, terminated: bool) -> u64 {
    newlines + u64::from(!terminated)
}
