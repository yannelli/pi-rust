//! Strict JSONL framing (upstream `modes/rpc/jsonl.ts`): a record ends at LF only, an optional
//! CR before it is dropped, and U+2028 and U+2029 stay inside records as they are valid JSON.

use std::collections::VecDeque;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};

/// One record: compact JSON followed by LF. Unicode separators are not escaped.
pub fn serialize_json_line(value: &Value) -> String {
    let mut line = value.to_string();
    line.push('\n');
    line
}

fn decode_line(bytes: &[u8]) -> String {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// Splits a byte stream into lines on LF only, across chunk boundaries.
#[derive(Debug, Default)]
pub struct JsonlSplitter {
    buffer: Vec<u8>,
    /// Bytes of `buffer` already searched for LF.
    scanned: usize,
}

impl JsonlSplitter {
    /// Add a chunk and return the lines it completes.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut lines = Vec::new();
        let mut start = 0;
        let mut search = self.scanned;
        while let Some(offset) = self.buffer[search..].iter().position(|byte| *byte == b'\n') {
            let end = search + offset;
            lines.push(decode_line(&self.buffer[start..end]));
            start = end + 1;
            search = start;
        }
        self.buffer.drain(..start);
        self.scanned = self.buffer.len();
        lines
    }

    /// The unterminated last line at end of input, if any.
    pub fn finish(&mut self) -> Option<String> {
        self.scanned = 0;
        if self.buffer.is_empty() {
            return None;
        }
        let line = decode_line(&self.buffer);
        self.buffer.clear();
        Some(line)
    }
}

/// Reads LF-delimited records from an async byte stream.
pub struct JsonlReader<R> {
    reader: R,
    splitter: JsonlSplitter,
    ready: VecDeque<String>,
    done: bool,
}

impl<R: AsyncRead + Unpin> JsonlReader<R> {
    pub fn new(reader: R) -> Self {
        Self { reader, splitter: JsonlSplitter::default(), ready: VecDeque::new(), done: false }
    }

    /// The next record, or `None` at end of input. A final record without LF is returned too.
    pub async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            if let Some(line) = self.ready.pop_front() {
                return Ok(Some(line));
            }
            if self.done {
                return Ok(None);
            }
            let read = match self.reader.read(&mut chunk).await {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if read == 0 {
                self.done = true;
                self.ready.extend(self.splitter.finish());
            } else {
                self.ready.extend(self.splitter.push(&chunk[..read]));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn serializes_without_escaping_unicode_separators() {
        let line = serialize_json_line(&json!({"text": "a\u{2028}b\u{2029}c"}));
        assert!(line.contains("a\u{2028}b\u{2029}c"));
        assert!(line.ends_with('\n'));
        assert_eq!(serde_json::from_str::<Value>(line.trim()).unwrap(), json!({"text": "a\u{2028}b\u{2029}c"}));
    }

    #[test]
    fn splits_on_lf_only_across_chunks() {
        let mut splitter = JsonlSplitter::default();
        let record = serialize_json_line(&json!({"text": "a\u{2028}b\u{2029}c"}));
        let (first, second) = record.as_bytes().split_at(5);
        assert!(splitter.push(first).is_empty());
        let lines = splitter.push(second);
        assert_eq!(lines.len(), 1);
        assert_eq!(serde_json::from_str::<Value>(&lines[0]).unwrap()["text"], "a\u{2028}b\u{2029}c");
        assert_eq!(splitter.finish(), None);

        let euro = "\u{20ac}".as_bytes();
        assert!(splitter.push(&[b'"', euro[0]]).is_empty());
        assert_eq!(splitter.push(&[euro[1], euro[2], b'"', b'\n']), ["\"\u{20ac}\""]);
    }

    #[test]
    fn strips_cr_and_returns_a_final_line_without_lf() {
        let mut splitter = JsonlSplitter::default();
        assert_eq!(splitter.push(b"{\"a\":1}\r\n{\"b\":2}\r\n"), ["{\"a\":1}", "{\"b\":2}"]);
        assert_eq!(splitter.push(b"{\"a\":1}"), Vec::<String>::new());
        assert_eq!(splitter.finish().as_deref(), Some("{\"a\":1}"));
        assert_eq!(splitter.push(b"\n\r\n"), ["", ""]);
    }

    #[tokio::test]
    async fn reader_yields_records_then_end_of_input() {
        let input: &[u8] = b"{\"a\":1}\n{\"b\":\"x\r\"}\r\ntail";
        let mut reader = JsonlReader::new(input);
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("{\"a\":1}"));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("{\"b\":\"x\r\"}"));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("tail"));
        assert_eq!(reader.next_line().await.unwrap(), None);
    }
}
