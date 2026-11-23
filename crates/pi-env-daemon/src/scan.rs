//! Port of Durable's `LineScanner` (`env/line-scan.ts`): locates lines `[start_line, end_line)` of a file fed in order
//! and measures their decoded UTF-8 size as `new TextDecoder().decode(file)` would produce it.

use crate::decode::{StreamDecoder, count_newlines, starts_with_bom};
use serde_json::{Value, json};

const NEWLINE: u8 = 0x0a;
/// Chunks are scanned in blocks of this size: a block in which no line of the selection starts or ends is measured
/// whole, the others line by line.
const BLOCK: usize = 16 * 1024;

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
    head: Option<Vec<u8>>,
    bom: bool,
}

impl LineScanner {
    /// `end_line`, when given, must be greater than `start_line`.
    pub fn new(start_line: u64, end_line: Option<u64>) -> LineScanner {
        let mut scanner = LineScanner {
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
            head: Some(Vec::with_capacity(3)),
            bom: false,
        };
        if start_line == 0 {
            scanner.begin(0);
        }
        scanner
    }

    fn is_last_line(&self, line: u64) -> bool {
        self.end_line.is_some_and(|end| line + 1 == end)
    }

    pub fn push(&mut self, mut chunk: &[u8]) {
        if let Some(head) = &mut self.head {
            let take = (3 - head.len()).min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
            if head.len() < 3 {
                return;
            }
            chunk = &chunk[take..];
            self.release_head();
        }
        self.process(chunk);
    }

    fn release_head(&mut self) {
        let head = self.head.take().unwrap_or_default();
        self.bom = starts_with_bom(&head);
        self.process(&head);
    }

    fn process(&mut self, chunk: &[u8]) {
        for block in chunk.chunks(BLOCK) {
            let count = count_newlines(block) as u64;
            if self.reaches_an_edge(count) {
                self.process_lines(block);
                continue;
            }
            let base = self.position;
            self.feed(block, base, 0, block.len());
            if count > 0
                && let Some(last) = block.iter().rposition(|byte| *byte == NEWLINE)
            {
                self.newlines += count;
                self.line_start = base + last as u64 + 1;
            }
            self.position += block.len() as u64;
        }
    }

    /// Whether one of the next `count` newlines ends line `start_line - 1`, `start_line`, `end_line - 2`, or
    /// `end_line - 1`: the newlines at which `process_lines` begins the selection, ends its first line, begins its last
    /// line, or ends it.
    fn reaches_an_edge(&self, count: u64) -> bool {
        let lines = self.newlines..self.newlines + count;
        let last = self.end_line.and_then(|end| end.checked_sub(1));
        [self.start_line.checked_sub(1), Some(self.start_line), last.and_then(|line| line.checked_sub(1)), last]
            .into_iter()
            .flatten()
            .any(|line| lines.contains(&line))
    }

    fn process_lines(&mut self, chunk: &[u8]) {
        let base = self.position;
        let mut from = 0;
        let mut index = 0;
        while let Some(found) = chunk[index..].iter().position(|byte| *byte == NEWLINE) {
            let newline = index + found;
            // The newline ends line `self.newlines`; it belongs to the selection between selected lines only.
            self.feed(chunk, base, from, newline);
            let line = self.newlines;
            let position = base + newline as u64;
            if line == self.start_line {
                self.end_first_line(position);
            }
            if self.is_last_line(line) {
                self.end_selection(position);
            }
            self.feed(chunk, base, newline, newline + 1);
            from = newline + 1;
            self.newlines += 1;
            self.line_start = position + 1;
            if self.newlines == self.start_line {
                self.begin(self.line_start);
            }
            if self.is_last_line(self.newlines) {
                self.last_line_start = Some(self.line_start);
            }
            index = newline + 1;
        }
        self.feed(chunk, base, from, chunk.len());
        self.position += chunk.len() as u64;
    }

    pub fn finish(mut self) -> Value {
        if self.head.is_some() {
            self.release_head();
        }
        let size = self.position;
        let Some(start) = self.start else {
            return json!({
                "newlines": self.newlines, "start": size, "end": size, "firstLineEnd": size,
                "lastLineStart": size, "selectedBytes": 0, "firstLineBytes": 0,
            });
        };
        if self.first_line_end.is_none() {
            self.end_first_line(size);
        }
        if self.end.is_none() {
            self.end_selection(size);
        }
        json!({
            "newlines": self.newlines,
            "start": start,
            "end": self.end.unwrap_or(size),
            "firstLineEnd": self.first_line_end.unwrap_or(size),
            // A selection that reaches past the last line ends with the last line.
            "lastLineStart": self.last_line_start.unwrap_or(self.line_start),
            "selectedBytes": self.selected_bytes,
            "firstLineBytes": self.first_line_bytes,
        })
    }

    fn begin(&mut self, start: u64) {
        self.start = Some(start);
        if self.is_last_line(self.start_line) {
            self.last_line_start = Some(start);
        }
        self.selection = Some(StreamDecoder::range());
    }

    fn end_first_line(&mut self, position: u64) {
        self.first_line_end = Some(position);
        if let Some(decoder) = &self.selection {
            self.first_line_bytes = self.selected_bytes + decoder.finish_len() as u64;
        }
    }

    fn end_selection(&mut self, position: u64) {
        self.end = Some(position);
        if let Some(decoder) = self.selection.take() {
            self.selected_bytes += decoder.finish_len() as u64;
        }
    }

    fn feed(&mut self, chunk: &[u8], base: u64, mut from: usize, to: usize) {
        // Decoding the whole file drops a leading byte-order mark.
        if self.bom && base + (from as u64) < 3 {
            from = to.min((3 - base) as usize);
        }
        if to <= from {
            return;
        }
        let bytes = &chunk[from..to];
        if let Some(decoder) = &mut self.selection {
            self.selected_bytes += decoder.decoded_len(bytes) as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `LineScanner` before the whole-block path, which went through every chunk line by line.
    struct OldScanner {
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
        head: Option<Vec<u8>>,
        bom: bool,
    }

    impl OldScanner {
        fn new(start_line: u64, end_line: Option<u64>) -> OldScanner {
            let mut scanner = OldScanner {
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
                head: Some(Vec::with_capacity(3)),
                bom: false,
            };
            if start_line == 0 {
                scanner.begin(0);
            }
            scanner
        }

        fn is_last_line(&self, line: u64) -> bool {
            self.end_line.is_some_and(|end| line + 1 == end)
        }

        fn push(&mut self, mut chunk: &[u8]) {
            if let Some(head) = &mut self.head {
                let take = (3 - head.len()).min(chunk.len());
                head.extend_from_slice(&chunk[..take]);
                if head.len() < 3 {
                    return;
                }
                chunk = &chunk[take..];
                self.release_head();
            }
            self.process(chunk);
        }

        fn release_head(&mut self) {
            let head = self.head.take().unwrap_or_default();
            self.bom = starts_with_bom(&head);
            self.process(&head);
        }

        fn process(&mut self, chunk: &[u8]) {
            let base = self.position;
            let mut from = 0;
            let mut index = 0;
            while let Some(found) = chunk[index..].iter().position(|byte| *byte == NEWLINE) {
                let newline = index + found;
                self.feed(chunk, base, from, newline);
                let line = self.newlines;
                let position = base + newline as u64;
                if line == self.start_line {
                    self.end_first_line(position);
                }
                if self.is_last_line(line) {
                    self.end_selection(position);
                }
                self.feed(chunk, base, newline, newline + 1);
                from = newline + 1;
                self.newlines += 1;
                self.line_start = position + 1;
                if self.newlines == self.start_line {
                    self.begin(self.line_start);
                }
                if self.is_last_line(self.newlines) {
                    self.last_line_start = Some(self.line_start);
                }
                index = newline + 1;
            }
            self.feed(chunk, base, from, chunk.len());
            self.position += chunk.len() as u64;
        }

        fn finish(mut self) -> Value {
            if self.head.is_some() {
                self.release_head();
            }
            let size = self.position;
            let Some(start) = self.start else {
                return json!({
                    "newlines": self.newlines, "start": size, "end": size, "firstLineEnd": size,
                    "lastLineStart": size, "selectedBytes": 0, "firstLineBytes": 0,
                });
            };
            if self.first_line_end.is_none() {
                self.end_first_line(size);
            }
            if self.end.is_none() {
                self.end_selection(size);
            }
            json!({
                "newlines": self.newlines,
                "start": start,
                "end": self.end.unwrap_or(size),
                "firstLineEnd": self.first_line_end.unwrap_or(size),
                "lastLineStart": self.last_line_start.unwrap_or(self.line_start),
                "selectedBytes": self.selected_bytes,
                "firstLineBytes": self.first_line_bytes,
            })
        }

        fn begin(&mut self, start: u64) {
            self.start = Some(start);
            if self.is_last_line(self.start_line) {
                self.last_line_start = Some(start);
            }
            self.selection = Some(StreamDecoder::range());
        }

        fn end_first_line(&mut self, position: u64) {
            self.first_line_end = Some(position);
            if let Some(decoder) = &self.selection {
                self.first_line_bytes = self.selected_bytes + decoder.finish_len() as u64;
            }
        }

        fn end_selection(&mut self, position: u64) {
            self.end = Some(position);
            if let Some(decoder) = self.selection.take() {
                self.selected_bytes += decoder.finish_len() as u64;
            }
        }

        fn feed(&mut self, chunk: &[u8], base: u64, mut from: usize, to: usize) {
            if self.bom && base + (from as u64) < 3 {
                from = to.min((3 - base) as usize);
            }
            if to <= from {
                return;
            }
            let bytes = &chunk[from..to];
            if let Some(decoder) = &mut self.selection {
                self.selected_bytes += decoder.decoded_len(bytes) as u64;
            }
        }
    }

    fn scan(file: &[u8], start: u64, end: Option<u64>, chunk: usize) -> Value {
        let mut scanner = LineScanner::new(start, end);
        for piece in file.chunks(chunk.max(1)) {
            scanner.push(piece);
        }
        scanner.finish()
    }

    #[test]
    fn locates_and_measures_lines_like_whole_file_decoding() {
        // BOM, "a", invalid sequence, empty line, a later U+FEFF, and no final newline.
        let file = [0xef, 0xbb, 0xbf, 0x61, 0x0a, 0xe2, 0x82, 0x0a, 0x0a, 0xef, 0xbb, 0xbf, 0x62, 0x0a, 0xc3, 0xa9];
        for chunk in 1..6 {
            let all = scan(&file, 0, None, chunk);
            assert_eq!(all["newlines"], 4);
            // "a\n\u{fffd}\n\n\u{feff}b\né" is 1+1+3+1+1+3+1+1+2 = 14 bytes.
            assert_eq!(all["selectedBytes"], 14);
            assert_eq!(all["firstLineBytes"], 1);
            let middle = scan(&file, 1, Some(3), chunk);
            assert_eq!((middle["start"].as_u64(), middle["end"].as_u64()), (Some(5), Some(8)));
            assert_eq!(middle["selectedBytes"], 4);
            assert_eq!(middle["lastLineStart"], 8);
            let past = scan(&file, 9, None, chunk);
            assert_eq!(past["start"], 16);
        }
    }

    /// Files a few blocks long: lines with multi-byte characters, invalid and truncated sequences, U+FEFF, CRLF, and
    /// empty lines, with one line longer than a block; the same with a byte-order mark and no final newline; one line
    /// with no newline; only newlines; and files shorter than a byte-order mark.
    fn files() -> Vec<Vec<u8>> {
        const PIECES: [&[u8]; 12] = [
            b"\n",
            b"\n",
            b"\r\n",
            b"word ",
            b"\xc3\xa9",
            b"\xe2\x82\xac",
            b"\xf0\x9f\x98\x80",
            b"\xef\xbb\xbf",
            b"\xe2\x82",
            b"\xff",
            b"\x80",
            b"\xf0\x9f",
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut lines = Vec::new();
        while lines.len() < BLOCK + BLOCK / 4 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            lines.extend_from_slice(PIECES[(state % PIECES.len() as u64) as usize]);
        }
        let long = "\u{20ac}\u{65e5}\u{1f600}".repeat(BLOCK / 10 + 1);
        lines.splice(BLOCK / 2..BLOCK / 2, long.bytes());
        let marked = [b"\xef\xbb\xbf", lines.trim_ascii_end()].concat();
        let one_line = [long.as_bytes(), b"\xff", long.as_bytes()].concat();
        let newlines = vec![NEWLINE; BLOCK + BLOCK / 2];
        let mut files = vec![lines, marked, one_line, newlines];
        files.extend([&b""[..], b"\xef\xbb", b"\xef\xbb\xbf", b"\xef\xbb\xbf\n\xe2\x82"].map(<[u8]>::to_vec));
        files
    }

    /// Ranges that start and end at the first lines, around each block boundary, at the last lines, and past them.
    fn ranges(file: &[u8]) -> Vec<(u64, Option<u64>)> {
        let line_at = |offset: usize| count_newlines(&file[..offset.min(file.len())]) as u64;
        let last = line_at(file.len());
        let mut lines = vec![0, 1, 2, last.saturating_sub(1), last, last + 1, last + 2];
        for offset in [BLOCK, BLOCK + 3, 2 * BLOCK + 3] {
            let line = line_at(offset);
            lines.extend([line.saturating_sub(1), line, line + 1]);
        }
        lines.sort_unstable();
        lines.dedup();
        let mut ranges = Vec::new();
        for &start in &lines {
            ranges.extend([(start, None), (start, Some(start + 1)), (start, Some(start + 2))]);
            ranges.extend(lines.iter().filter(|&&end| end > start + 2).map(|&end| (start, Some(end))));
        }
        ranges
    }

    #[test]
    fn whole_blocks_scan_like_the_line_by_line_scanner() {
        for (index, whole) in files().iter().enumerate() {
            for piece in [1, 2, 3, 7, 4093, BLOCK, BLOCK + 3, 64 * 1024] {
                // A piece shorter than a block is a block: the start of the file has lines enough for it.
                let file = if piece < 8 { &whole[..whole.len().min(400)] } else { whole };
                for (start, end) in ranges(file) {
                    let mut scanner = LineScanner::new(start, end);
                    let mut old = OldScanner::new(start, end);
                    for part in file.chunks(piece) {
                        scanner.push(part);
                        old.push(part);
                    }
                    let message = format!("file {index}, lines {start}..{end:?}, pieces of {piece}");
                    assert_eq!(scanner.finish(), old.finish(), "{message}");
                }
            }
        }
    }
}
