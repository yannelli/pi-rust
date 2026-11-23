//! Durable's output window (`ShellExecOptions.window`, `ShellOutputInfo.skipped`): undelivered command output is held
//! and coalesced; text followed by more than the window (by a byte or a line) can never be in the caller's tail and is
//! replaced by counts. Delivery follows the caller's commit pace, and waits while an earlier frame is still unsent.

use crate::decode::count_newlines;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A chunk takes the next chunk of its stream in while both fit in this many bytes, so output written a few bytes at
/// a time does not hold one allocation per write.
const JOIN: usize = 4096;

#[derive(Clone, Copy)]
pub struct Window {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub min_interval: Duration,
    pub bytes_per_second: f64,
}

impl Window {
    pub fn from_json(json: &Value) -> Option<Window> {
        let object = json.as_object()?;
        Some(Window {
            max_bytes: object.get("maxBytes")?.as_u64()? as usize,
            max_lines: object.get("maxLines")?.as_u64()? as usize,
            min_interval: Duration::from_secs_f64(object.get("minIntervalMs")?.as_f64()?.max(0.0) / 1000.0),
            bytes_per_second: object.get("bytesPerSecond")?.as_f64()?.max(1.0),
        })
    }
}

#[derive(Default)]
struct Skipped {
    bytes: u64,
    newlines: u64,
    ends_with_newline: bool,
}

/// Decoded text of one stream as it arrived; the part still held starts at `from`.
struct Chunk {
    stream: &'static str,
    text: Vec<u8>,
    from: usize,
    /// Newlines in the part still held.
    newlines: usize,
}

impl Chunk {
    fn held(&self) -> &[u8] {
        &self.text[self.from..]
    }
}

/// One output event: its stream, text (valid UTF-8), and the output omitted right before it.
pub struct Event {
    pub stream: &'static str,
    pub text: Vec<u8>,
    pub skipped: Option<Value>,
}

pub struct Pending {
    window: Window,
    /// Undelivered text in arrival order.
    chunks: VecDeque<Chunk>,
    bytes: usize,
    newlines: usize,
    skipped: Option<Skipped>,
    next_send: Instant,
}

impl Pending {
    pub fn new(window: Window) -> Pending {
        Pending { window, chunks: VecDeque::new(), bytes: 0, newlines: 0, skipped: None, next_send: Instant::now() }
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// When the next delivery may happen.
    pub fn next_send(&self) -> Instant {
        self.next_send
    }

    /// `text` is valid UTF-8.
    pub fn push(&mut self, stream: &'static str, text: Vec<u8>) {
        if text.is_empty() {
            return;
        }
        let newlines = count_newlines(&text);
        self.bytes += text.len();
        self.newlines += newlines;
        match self.chunks.back_mut() {
            Some(last) if last.stream == stream && last.text.len() + text.len() <= JOIN => {
                last.text.extend_from_slice(&text);
                last.newlines += newlines;
            }
            _ => self.chunks.push_back(Chunk { stream, text, from: 0, newlines }),
        }
        // Trimming costs a pass over one chunk; do it once a few windows have accumulated.
        if self.bytes > 4 * (self.window.max_bytes + 1) || self.newlines > 4 * (self.window.max_lines + 1) {
            self.trim();
        }
    }

    /// Whether the text after a cut still proves the cut text is outside the window.
    fn proves(&self, bytes: usize, newlines: usize) -> bool {
        bytes > self.window.max_bytes || newlines > self.window.max_lines
    }

    fn skip(&mut self, bytes: usize, newlines: usize, ends_with_newline: bool) {
        self.bytes -= bytes;
        self.newlines -= newlines;
        let skipped = self.skipped.get_or_insert_with(Skipped::default);
        skipped.bytes += bytes as u64;
        skipped.newlines += newlines as u64;
        skipped.ends_with_newline = ends_with_newline;
    }

    /// Replace the longest prefix whose remainder still proves exclusion by counts. Chunks start at character
    /// boundaries, so the cut does not depend on how the text was chunked.
    fn trim(&mut self) {
        // Whole chunks first, by their counts.
        while self.chunks.len() > 1 {
            let first = &self.chunks[0];
            let (bytes, newlines) = (first.held().len(), first.newlines);
            if !self.proves(self.bytes - bytes, self.newlines - newlines) {
                break;
            }
            let ends_with_newline = first.text.last() == Some(&b'\n');
            self.chunks.pop_front();
            self.skip(bytes, newlines, ends_with_newline);
        }
        // Then a prefix of the first chunk left, at a character boundary.
        let first = &self.chunks[0];
        let held = first.held();
        let rest_bytes = self.bytes - held.len();
        let rest_newlines = self.newlines - first.newlines;
        let mut cut = 0;
        // By bytes: keep more than `max_bytes` overall.
        let keep_bytes = (self.window.max_bytes + 1).saturating_sub(rest_bytes);
        if held.len() > keep_bytes {
            let mut index = held.len() - keep_bytes;
            while index > 0 && held.get(index).is_some_and(|byte| byte & 0xc0 == 0x80) {
                index -= 1;
            }
            cut = index;
        }
        // By lines: keep more than `max_lines` newlines overall, starting at a newline of this chunk.
        let keep_newlines = (self.window.max_lines + 1).saturating_sub(rest_newlines);
        if keep_newlines == 0 {
            cut = held.len();
        } else if first.newlines >= keep_newlines {
            let mut newlines = held.iter().enumerate().rev().filter(|(_, byte)| **byte == b'\n');
            if let Some((index, _)) = newlines.nth(keep_newlines - 1) {
                cut = cut.max(index);
            }
        }
        if cut > 0 {
            let removed = &held[..cut];
            let (newlines, ends_with_newline) = (count_newlines(removed), removed.last() == Some(&b'\n'));
            let first = &mut self.chunks[0];
            first.from += cut;
            first.newlines -= newlines;
            if first.from == first.text.len() {
                self.chunks.pop_front();
            }
            self.skip(cut, newlines, ends_with_newline);
        }
    }

    /// Deliver everything held: one event carrying the skip and all text after it, or one event per run of a stream.
    pub fn take(&mut self) -> Vec<Event> {
        let chunks = std::mem::take(&mut self.chunks);
        let delivered = self.bytes;
        self.bytes = 0;
        self.newlines = 0;
        // The caller writes at most the window per sample, and paces itself by what it wrote.
        let pace = Duration::from_secs_f64(delivered.min(self.window.max_bytes) as f64 / self.window.bytes_per_second);
        self.next_send = Instant::now() + self.window.min_interval.max(pace);
        match self.skipped.take() {
            Some(skipped) => {
                let stream = chunks.back().map_or("stdout", |chunk| chunk.stream);
                let mut text = Vec::with_capacity(delivered);
                for chunk in &chunks {
                    text.extend_from_slice(chunk.held());
                }
                vec![Event {
                    stream,
                    text,
                    skipped: Some(json!({
                        "bytes": skipped.bytes,
                        "newlines": skipped.newlines,
                        "endsWithNewline": skipped.ends_with_newline,
                    })),
                }]
            }
            None => {
                let mut events: Vec<Event> = Vec::new();
                for chunk in chunks {
                    match events.last_mut() {
                        Some(event) if event.stream == chunk.stream => event.text.extend_from_slice(chunk.held()),
                        _ => {
                            let mut text = chunk.text;
                            text.drain(..chunk.from);
                            events.push(Event { stream: chunk.stream, text, skipped: None });
                        }
                    }
                }
                events
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(max_bytes: usize, max_lines: usize) -> Window {
        Window { max_bytes, max_lines, min_interval: Duration::ZERO, bytes_per_second: 1e12 }
    }

    /// Bytes and newlines delivered or skipped, and the text after the last skip.
    fn total(events: &[Event]) -> (u64, u64, Vec<u8>) {
        let mut bytes = 0;
        let mut newlines = 0;
        let mut after = Vec::new();
        for event in events {
            if let Some(skipped) = &event.skipped {
                bytes += skipped["bytes"].as_u64().unwrap();
                newlines += skipped["newlines"].as_u64().unwrap();
                after.clear();
            }
            bytes += event.text.len() as u64;
            newlines += count_newlines(&event.text) as u64;
            after.extend_from_slice(&event.text);
        }
        (bytes, newlines, after)
    }

    #[test]
    fn counts_everything_and_keeps_more_than_the_window_after_a_skip() {
        let mut pending = Pending::new(window(10, 2));
        let mut full = Vec::new();
        let mut events = Vec::new();
        for index in 0..500 {
            let text = format!("line {index} é😀\n");
            full.extend_from_slice(text.as_bytes());
            pending.push(if index % 7 == 0 { "stderr" } else { "stdout" }, text.into_bytes());
            if index % 50 == 49 {
                events.extend(pending.take());
            }
        }
        events.extend(pending.take());
        let (bytes, newlines, after) = total(&events);
        assert_eq!(bytes, full.len() as u64);
        assert_eq!(newlines, count_newlines(&full) as u64);
        assert!(full.ends_with(&after));
        assert!(after.len() > 10 || count_newlines(&after) > 2);
        assert!(events.iter().any(|event| event.skipped.is_some()));
    }

    #[test]
    fn delivers_streams_apart_without_skips() {
        let mut pending = Pending::new(window(1000, 1000));
        pending.push("stdout", b"a".to_vec());
        pending.push("stderr", b"b".to_vec());
        pending.push("stdout", b"c".to_vec());
        let events = pending.take();
        let streams: Vec<_> = events.iter().map(|event| event.stream).collect();
        assert_eq!(streams, ["stdout", "stderr", "stdout"]);
    }

    #[test]
    fn joins_small_writes_of_a_stream() {
        let mut pending = Pending::new(window(1 << 20, 1 << 20));
        for _ in 0..10_000 {
            pending.push("stdout", b"x".to_vec());
        }
        assert_eq!(pending.chunks.len(), 10_000usize.div_ceil(JOIN));
    }

    #[test]
    fn reports_the_newline_that_ends_a_skipped_chunk() {
        let mut pending = Pending::new(window(5, 100));
        pending.push("stdout", b"abcdefghijklmnopqr\n".to_vec());
        pending.push("stderr", b"123456".to_vec());
        let events = pending.take();
        assert_eq!(events.len(), 1);
        assert_eq!((events[0].stream, events[0].text.as_slice()), ("stderr", &b"123456"[..]));
        assert_eq!(events[0].skipped, Some(json!({ "bytes": 19, "newlines": 1, "endsWithNewline": true })));
    }

    /// The window as it was before it held bytes, to compare against.
    mod old {
        use super::super::{Skipped, Window};
        use serde_json::{Value, json};
        use std::time::{Duration, Instant};

        pub struct Event {
            pub stream: &'static str,
            pub text: String,
            pub skipped: Option<Value>,
        }

        pub struct Pending {
            window: Window,
            /// Undelivered text in arrival order, by stream.
            segments: Vec<(&'static str, String)>,
            bytes: usize,
            newlines: usize,
            skipped: Option<Skipped>,
            next_send: Instant,
        }

        fn newline_count(text: &str) -> usize {
            text.bytes().filter(|byte| *byte == b'\n').count()
        }

        impl Pending {
            pub fn new(window: Window) -> Pending {
                Pending {
                    window,
                    segments: Vec::new(),
                    bytes: 0,
                    newlines: 0,
                    skipped: None,
                    next_send: Instant::now(),
                }
            }

            pub fn is_empty(&self) -> bool {
                self.segments.is_empty()
            }

            pub fn push(&mut self, stream: &'static str, text: String) {
                if text.is_empty() {
                    return;
                }
                self.bytes += text.len();
                self.newlines += newline_count(&text);
                match self.segments.last_mut() {
                    Some((last, existing)) if *last == stream => existing.push_str(&text),
                    _ => self.segments.push((stream, text)),
                }
                // Trimming costs a pass over the held text; do it once a few windows have accumulated.
                if self.bytes > 4 * (self.window.max_bytes + 1) || self.newlines > 4 * (self.window.max_lines + 1) {
                    self.trim();
                }
            }

            /// Whether the text after a cut still proves the cut text is outside the window.
            fn proves(&self, bytes: usize, newlines: usize) -> bool {
                bytes > self.window.max_bytes || newlines > self.window.max_lines
            }

            /// Replace the longest prefix whose remainder still proves exclusion by counts.
            fn trim(&mut self) {
                let mut removed = String::new();
                // Whole segments first.
                while self.segments.len() > 1 {
                    let (bytes, newlines) = {
                        let first = &self.segments[0].1;
                        (first.len(), newline_count(first))
                    };
                    if !self.proves(self.bytes - bytes, self.newlines - newlines) {
                        break;
                    }
                    let (_, text) = self.segments.remove(0);
                    self.bytes -= bytes;
                    self.newlines -= newlines;
                    removed.push_str(&text);
                }
                // Then a prefix of the first remaining segment, at a character boundary.
                let rest_bytes = self.bytes - self.segments[0].1.len();
                let rest_newlines = self.newlines - newline_count(&self.segments[0].1);
                let first = &self.segments[0].1;
                let mut cut = 0;
                // By bytes: keep more than `max_bytes` overall.
                let keep_bytes = (self.window.max_bytes + 1).saturating_sub(rest_bytes);
                if first.len() > keep_bytes {
                    let mut index = first.len() - keep_bytes;
                    while index > 0 && !first.is_char_boundary(index) {
                        index -= 1;
                    }
                    cut = cut.max(index);
                }
                // By lines: keep more than `max_lines` newlines overall, starting at a newline of this segment.
                let keep_newlines = (self.window.max_lines + 1).saturating_sub(rest_newlines);
                if keep_newlines > 0 {
                    let positions: Vec<usize> =
                        first.bytes().enumerate().filter(|(_, byte)| *byte == b'\n').map(|(index, _)| index).collect();
                    if positions.len() >= keep_newlines {
                        cut = cut.max(positions[positions.len() - keep_newlines]);
                    }
                } else {
                    cut = first.len();
                }
                if cut > 0 {
                    let first = &mut self.segments[0].1;
                    let prefix: String = first.drain(..cut).collect();
                    self.bytes -= prefix.len();
                    self.newlines -= newline_count(&prefix);
                    removed.push_str(&prefix);
                    if first.is_empty() {
                        self.segments.remove(0);
                    }
                }
                if !removed.is_empty() {
                    let skipped = self.skipped.get_or_insert_with(Skipped::default);
                    skipped.bytes += removed.len() as u64;
                    skipped.newlines += newline_count(&removed) as u64;
                    skipped.ends_with_newline = removed.ends_with('\n');
                }
            }

            /// Deliver everything held: one event carrying the skip and all text after it, or one event per segment.
            pub fn take(&mut self) -> Vec<Event> {
                let segments = std::mem::take(&mut self.segments);
                let delivered = self.bytes;
                self.bytes = 0;
                self.newlines = 0;
                // The caller writes at most the window per sample, and paces itself by what it wrote.
                let pace =
                    Duration::from_secs_f64(delivered.min(self.window.max_bytes) as f64 / self.window.bytes_per_second);
                self.next_send = Instant::now() + self.window.min_interval.max(pace);
                match self.skipped.take() {
                    Some(skipped) => {
                        let stream = segments.last().map(|(stream, _)| *stream).unwrap_or("stdout");
                        let text: String = segments.into_iter().map(|(_, text)| text).collect();
                        vec![Event {
                            stream,
                            text,
                            skipped: Some(json!({
                                "bytes": skipped.bytes,
                                "newlines": skipped.newlines,
                                "endsWithNewline": skipped.ends_with_newline,
                            })),
                        }]
                    }
                    None => segments.into_iter().map(|(stream, text)| Event { stream, text, skipped: None }).collect(),
                }
            }
        }
    }

    #[test]
    fn sends_the_events_the_text_window_sent() {
        let pieces = ["a", "bc", "\n", "\u{e9}", "\u{20ac}", "\u{1f600}", "line\n", "\n\n"];
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut below = |count: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % count as u64) as usize
        };
        for (max_bytes, max_lines) in [(0, 0), (1, 0), (0, 1), (3, 1), (10, 2), (100, 5), (1000, 50), (5000, 1000)] {
            let mut old = old::Pending::new(window(max_bytes, max_lines));
            let mut new = Pending::new(window(max_bytes, max_lines));
            let (mut expected, mut actual) = (Vec::new(), Vec::new());
            for round in 0..600 {
                let stream = if below(4) == 0 { "stderr" } else { "stdout" };
                // Writes of a character or two, lines, and chunks larger than `JOIN`.
                let count = [1, 1, 2, 10, 200, 2000][below(6)];
                let text: String = (0..count).map(|_| pieces[below(pieces.len())]).collect();
                old.push(stream, text.clone());
                new.push(stream, text.into_bytes());
                assert_eq!(old.is_empty(), new.is_empty());
                if (below(50) == 0 || round == 599) && !new.is_empty() {
                    expected.extend(
                        old.take().into_iter().map(|event| (event.stream, event.text.into_bytes(), event.skipped)),
                    );
                    actual.extend(new.take().into_iter().map(|event| (event.stream, event.text, event.skipped)));
                }
            }
            assert!(expected.iter().any(|(_, _, skipped)| skipped.is_some()), "window {max_bytes}/{max_lines}");
            assert!(expected == actual, "window {max_bytes}/{max_lines}");
        }
    }
}
