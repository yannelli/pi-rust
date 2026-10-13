//! `LineScanner` computes a `LineScan` from a file's bytes fed in order, so an environment can scan any size in
//! bounded memory. Decoded sizes use the same streaming decoder as decoding the whole file at once.

use std::fmt;

use super::decode::{StreamDecoder, starts_with_bom};
use super::types::LineScan;

const NEWLINE: u8 = 0x0a;

/// An empty line range: `end_line` must be greater than `start_line` (upstream `RangeError("Invalid line range")`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidLineRange;

impl fmt::Display for InvalidLineRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Invalid line range")
    }
}

impl std::error::Error for InvalidLineRange {}

/// Finds lines `[start_line, end_line)` of a stream of bytes; see `BinaryReader::scan_lines`.
#[derive(Debug)]
pub struct LineScanner {
    start_line: u64,
    end_line: Option<u64>,
    position: u64,
    newlines: u64,
    line_start: u64,
    start: Option<u64>,
    end: Option<u64>,
    first_line_end: Option<u64>,
    last_line_start: Option<u64>,
    selected_bytes: u64,
    first_line_bytes: u64,
    selection: Option<StreamDecoder>,
    first_line: Option<StreamDecoder>,
    /// The first bytes, held until it is known whether they are a byte-order mark.
    head: Option<Vec<u8>>,
    bom: bool,
}

impl LineScanner {
    /// `end_line` absent: to the end. Fails unless `end_line > start_line`.
    pub fn new(start_line: u64, end_line: Option<u64>) -> Result<Self, InvalidLineRange> {
        if end_line.is_some_and(|end| end <= start_line) {
            return Err(InvalidLineRange);
        }
        let mut scanner = Self {
            start_line,
            end_line,
            position: 0,
            newlines: 0,
            line_start: 0,
            start: None,
            end: None,
            first_line_end: None,
            last_line_start: None,
            selected_bytes: 0,
            first_line_bytes: 0,
            selection: None,
            first_line: None,
            head: Some(Vec::with_capacity(3)),
            bom: false,
        };
        if start_line == 0 {
            scanner.begin(0);
        }
        Ok(scanner)
    }

    pub fn push(&mut self, mut chunk: &[u8]) {
        if let Some(head) = self.head.as_mut() {
            let take = (3 - head.len()).min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
            if head.len() < 3 {
                return;
            }
            self.release_head();
            chunk = &chunk[take..];
        }
        self.process(chunk);
    }

    fn release_head(&mut self) {
        let head = self.head.take().unwrap_or_default();
        self.bom = starts_with_bom(&head);
        self.process(&head);
    }

    fn is_last_line(&self, line: u64) -> bool {
        self.end_line.is_some_and(|end| line == end - 1)
    }

    fn process(&mut self, chunk: &[u8]) {
        let base = self.position;
        let mut from = 0;
        let mut search = 0;
        while let Some(offset) = chunk[search..].iter().position(|&byte| byte == NEWLINE) {
            let index = search + offset;
            // The newline ends line `newlines`. It belongs to the selection between selected lines only.
            self.feed(chunk, base, from, index);
            let line = self.newlines;
            let position = base + index as u64;
            if line == self.start_line {
                self.end_first_line(position);
            }
            if self.is_last_line(line) {
                self.end_selection(position);
            }
            self.feed(chunk, base, index, index + 1);
            from = index + 1;
            search = index + 1;
            self.newlines += 1;
            self.line_start = position + 1;
            if self.newlines == self.start_line {
                self.begin(self.line_start);
            }
            if self.is_last_line(self.newlines) {
                self.last_line_start = Some(self.line_start);
            }
        }
        self.feed(chunk, base, from, chunk.len());
        self.position += chunk.len() as u64;
    }

    pub fn finish(mut self) -> LineScan {
        if self.head.is_some() {
            self.release_head();
        }
        let size = self.position;
        let Some(start) = self.start else {
            return LineScan {
                newlines: self.newlines,
                start: size,
                end: size,
                first_line_end: size,
                last_line_start: size,
                selected_bytes: 0,
                first_line_bytes: 0,
            };
        };
        if self.first_line_end.is_none() {
            self.end_first_line(size);
        }
        if self.end.is_none() {
            self.end_selection(size);
        }
        LineScan {
            newlines: self.newlines,
            start,
            end: self.end.unwrap_or(size),
            first_line_end: self.first_line_end.unwrap_or(size),
            // A selection that reaches past the last line ends with the last line.
            last_line_start: self.last_line_start.unwrap_or(self.line_start),
            selected_bytes: self.selected_bytes,
            first_line_bytes: self.first_line_bytes,
        }
    }

    fn begin(&mut self, start: u64) {
        self.start = Some(start);
        if self.is_last_line(self.start_line) {
            self.last_line_start = Some(start);
        }
        self.selection = Some(StreamDecoder::range());
        self.first_line = Some(StreamDecoder::range());
    }

    fn end_first_line(&mut self, position: u64) {
        self.first_line_end = Some(position);
        if let Some(mut decoder) = self.first_line.take() {
            self.first_line_bytes += decoder.finish().len() as u64;
        }
    }

    fn end_selection(&mut self, position: u64) {
        self.end = Some(position);
        if let Some(mut decoder) = self.selection.take() {
            self.selected_bytes += decoder.finish().len() as u64;
        }
    }

    /// Feed `chunk[from, to)`, which starts at file offset `base`, to the decoders of the ranges still open.
    fn feed(&mut self, chunk: &[u8], base: u64, mut from: usize, to: usize) {
        // Decoding the whole file drops a leading byte-order mark.
        if self.bom && base + (from as u64) < 3 {
            from = to.min((3 - base) as usize);
        }
        if to <= from {
            return;
        }
        let bytes = &chunk[from..to];
        if let Some(decoder) = self.selection.as_mut() {
            self.selected_bytes += decoder.decode(bytes).len() as u64;
        }
        if let Some(decoder) = self.first_line.as_mut() {
            self.first_line_bytes += decoder.decode(bytes).len() as u64;
        }
    }
}
