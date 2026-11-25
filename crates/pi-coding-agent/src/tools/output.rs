//! Streaming output accumulator with bounded memory and a temp file for the full output.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use crate::agent_session::output_file::OutputFile;
pub use crate::agent_session::output_file::OutputFileResult;

use super::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, TruncationResult, truncate_tail,
};

/// Incremental UTF-8 decoder that holds back incomplete trailing sequences.
#[derive(Default)]
struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    fn decode(&mut self, data: &[u8]) -> String {
        self.pending.extend_from_slice(data);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    out.push_str(s);
                    self.pending.clear();
                    return out;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    out.push_str(std::str::from_utf8(&self.pending[..valid]).unwrap_or_default());
                    match e.error_len() {
                        None => {
                            self.pending.drain(..valid);
                            return out;
                        }
                        Some(bad) => {
                            out.push('\u{FFFD}');
                            self.pending.drain(..valid + bad);
                        }
                    }
                }
            }
        }
    }

    fn finish(&mut self) -> String {
        let out = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        out
    }
}

pub struct OutputSnapshot {
    pub content: String,
    pub truncation: TruncationResult,
    /// Set once the full output goes to a file. While output streams in, `Saved` means the file
    /// is being written.
    pub full_output_file: Option<OutputFileResult>,
}

impl OutputSnapshot {
    /// The full output file, unless it could not be saved.
    pub fn saved_path(&self) -> Option<&std::path::Path> {
        self.full_output_file.as_ref().and_then(OutputFileResult::saved_path)
    }
}

/// The complete output, or its head and tail when it is too long.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FullOutput {
    pub content: String,
    /// Whether `content` omits part of the output.
    pub truncated: bool,
}

pub struct OutputAccumulator {
    max_lines: usize,
    max_bytes: usize,
    max_rolling_bytes: usize,
    temp_file_prefix: &'static str,
    decoder: Utf8Stream,
    raw_chunks: Vec<u8>,
    tail_text: String,
    tail_starts_at_line_boundary: bool,
    total_raw_bytes: usize,
    total_decoded_bytes: usize,
    completed_lines: usize,
    total_lines: usize,
    current_line_bytes: usize,
    temp_dir: PathBuf,
    temp_file: Option<OutputFile>,
}

impl OutputAccumulator {
    pub fn new(temp_file_prefix: &'static str) -> Self {
        Self::with_limits(temp_file_prefix, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)
    }

    pub fn with_limits(temp_file_prefix: &'static str, max_lines: usize, max_bytes: usize) -> Self {
        Self {
            max_lines,
            max_bytes,
            max_rolling_bytes: (max_bytes * 2).max(1),
            temp_file_prefix,
            decoder: Utf8Stream::default(),
            raw_chunks: Vec::new(),
            tail_text: String::new(),
            tail_starts_at_line_boundary: true,
            total_raw_bytes: 0,
            total_decoded_bytes: 0,
            completed_lines: 0,
            total_lines: 0,
            current_line_bytes: 0,
            temp_dir: std::env::temp_dir(),
            temp_file: None,
        }
    }

    /// Write the temp file to `dir` instead of the system temp directory.
    #[cfg(test)]
    pub(super) fn with_temp_dir(mut self, dir: &std::path::Path) -> Self {
        self.temp_dir = dir.to_path_buf();
        self
    }

    pub fn append(&mut self, data: &[u8]) {
        self.total_raw_bytes += data.len();
        let text = self.decoder.decode(data);
        self.append_decoded(&text);
        if self.temp_file.is_some() || self.should_use_temp_file() {
            self.ensure_temp_file().write(data);
        } else {
            self.raw_chunks.extend_from_slice(data);
        }
    }

    pub fn finish(&mut self) {
        let text = self.decoder.finish();
        self.append_decoded(&text);
        if self.should_use_temp_file() {
            self.ensure_temp_file();
        }
    }

    pub fn snapshot(&mut self, persist_if_truncated: bool) -> OutputSnapshot {
        let text = self.snapshot_text();
        let tail = truncate_tail(text, TruncationOptions { max_lines: self.max_lines, max_bytes: self.max_bytes });
        let truncated = self.total_lines > self.max_lines || self.total_decoded_bytes > self.max_bytes;
        let truncated_by = truncated.then(|| {
            tail.truncated_by.unwrap_or(if self.total_decoded_bytes > self.max_bytes {
                TruncatedBy::Bytes
            } else {
                TruncatedBy::Lines
            })
        });
        let truncation = TruncationResult {
            truncated,
            truncated_by,
            total_lines: self.total_lines,
            total_bytes: self.total_decoded_bytes,
            ..tail
        };
        if persist_if_truncated && truncated {
            self.ensure_temp_file();
        }
        let full_output_file = self.temp_file.as_ref().map(|file| file.result().clone());
        OutputSnapshot { content: truncation.content.clone(), truncation, full_output_file }
    }

    /// Close the temp file. A failed file shows up as `Failed` in later snapshots.
    pub fn close_temp_file(&mut self) {
        if let Some(file) = self.temp_file.as_mut() {
            file.close();
        }
    }

    /// The complete output for callers that take more than the display snapshot. Call after
    /// `finish` and `close_temp_file`. Output over `max_bytes` keeps its first and last
    /// `max_bytes / 2` bytes around an omission marker. When the temp file could not be saved,
    /// only the retained tail is left: up to its last `max_bytes` bytes, behind the same marker
    /// when output is missing.
    pub fn read_full_output(&self, max_bytes: usize) -> FullOutput {
        match self.temp_file.as_ref().map(OutputFile::result) {
            None => FullOutput { content: String::from_utf8_lossy(&self.raw_chunks).into_owned(), truncated: false },
            Some(OutputFileResult::Failed(_)) => self.read_retained_output(max_bytes),
            Some(OutputFileResult::Saved(path)) => read_file_output(path, max_bytes)
                .unwrap_or_else(|_| FullOutput { content: String::new(), truncated: false }),
        }
    }

    pub fn last_line_bytes(&self) -> usize {
        self.current_line_bytes
    }

    fn append_decoded(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.total_decoded_bytes += text.len();
        self.tail_text.push_str(text);
        if self.tail_text.len() > self.max_rolling_bytes * 2 {
            self.trim_tail();
        }
        match text.rfind('\n') {
            None => self.current_line_bytes += text.len(),
            Some(last) => {
                self.completed_lines += text.matches('\n').count();
                self.current_line_bytes = text.len() - last - 1;
            }
        }
        let has_open_line = !self.tail_text.ends_with('\n');
        self.total_lines = self.completed_lines + usize::from(has_open_line);
    }

    fn trim_tail(&mut self) {
        let mut start = self.tail_text.len() - self.max_rolling_bytes;
        while !self.tail_text.is_char_boundary(start) {
            start += 1;
        }
        if start > 0 {
            self.tail_starts_at_line_boundary = self.tail_text.as_bytes()[start - 1] == b'\n';
        }
        self.tail_text.drain(..start);
    }

    fn snapshot_text(&self) -> &str {
        if self.tail_starts_at_line_boundary {
            return &self.tail_text;
        }
        match self.tail_text.find('\n') {
            Some(i) => &self.tail_text[i + 1..],
            None => &self.tail_text,
        }
    }

    /// The retained tail within `max_bytes`, marked as truncated only when output is missing. When
    /// the clipped tail is blank, the end of the over-long line before it stays.
    fn read_retained_output(&self, max_bytes: usize) -> FullOutput {
        let clipped = self.snapshot_text();
        let retained = if clipped.trim().is_empty() { self.tail_text.as_str() } else { clipped };
        let mut start = retained.len().saturating_sub(max_bytes);
        while !retained.is_char_boundary(start) {
            start += 1;
        }
        let tail = &retained[start..];
        let omitted = self.total_decoded_bytes - tail.len();
        if omitted == 0 {
            return FullOutput { content: tail.to_string(), truncated: false };
        }
        FullOutput { content: format!("[... {omitted} bytes omitted ...]\n\n{tail}"), truncated: true }
    }

    fn should_use_temp_file(&self) -> bool {
        self.total_raw_bytes > self.max_bytes
            || self.total_decoded_bytes > self.max_bytes
            || self.total_lines > self.max_lines
    }

    fn ensure_temp_file(&mut self) -> &mut OutputFile {
        let file = match self.temp_file.take() {
            Some(file) => file,
            None => {
                let mut file = OutputFile::create_in(&self.temp_dir, self.temp_file_prefix, ".log");
                file.write(&std::mem::take(&mut self.raw_chunks));
                file
            }
        };
        self.temp_file.insert(file)
    }
}

/// Length of `bytes` without an incomplete UTF-8 sequence at the end.
fn complete_prefix_len(bytes: &[u8]) -> usize {
    let len = bytes.len();
    for back in 1..=len.min(4) {
        let byte = bytes[len - back];
        if byte & 0xc0 == 0x80 {
            continue;
        }
        let needed = match byte {
            0xf0.. => 4,
            0xe0.. => 3,
            0xc0.. => 2,
            _ => 1,
        };
        return if needed > back { len - back } else { len };
    }
    len
}

fn read_file_output(path: &std::path::Path, max_bytes: usize) -> std::io::Result<FullOutput> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len() as usize;
    if size <= max_bytes {
        let mut bytes = Vec::with_capacity(size);
        file.read_to_end(&mut bytes)?;
        return Ok(FullOutput { content: String::from_utf8_lossy(&bytes).into_owned(), truncated: false });
    }
    let head_bytes = max_bytes / 2;
    let tail_bytes = max_bytes - head_bytes;
    let mut head = vec![0; head_bytes];
    file.read_exact(&mut head)?;
    let mut tail = vec![0; tail_bytes];
    file.seek(SeekFrom::Start((size - tail_bytes) as u64))?;
    file.read_exact(&mut tail)?;
    // Cut at character boundaries: drop an incomplete trailing sequence of the head and
    // leading continuation bytes of the tail.
    let head_text = String::from_utf8_lossy(&head[..complete_prefix_len(&head)]);
    let tail_start = tail.iter().take_while(|byte| *byte & 0xc0 == 0x80).count();
    let tail_text = String::from_utf8_lossy(&tail[tail_start..]);
    let omitted = size - head_bytes - tail_bytes;
    let content = format!("{head_text}\n\n[... {omitted} bytes omitted ...]\n\n{tail_text}");
    Ok(FullOutput { content, truncated: true })
}
