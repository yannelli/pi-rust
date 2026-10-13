//! Streaming output accumulator with bounded memory and a temp file for the full output.

use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

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
    pub full_output_path: Option<PathBuf>,
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
    temp_path: Option<PathBuf>,
    temp_file: Option<File>,
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
            temp_path: None,
            temp_file: None,
        }
    }

    pub fn append(&mut self, data: &[u8]) {
        self.total_raw_bytes += data.len();
        let text = self.decoder.decode(data);
        self.append_decoded(&text);
        if self.temp_file.is_some() || self.should_use_temp_file() {
            self.ensure_temp_file();
            if let Some(file) = self.temp_file.as_mut() {
                let _ = file.write_all(data);
            }
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
        OutputSnapshot { content: truncation.content.clone(), truncation, full_output_path: self.temp_path.clone() }
    }

    pub fn close_temp_file(&mut self) {
        if let Some(mut file) = self.temp_file.take() {
            let _ = file.flush();
        }
    }

    /// The complete output for callers that take more than the display snapshot. Call after
    /// `finish` and `close_temp_file`. Output over `max_bytes` keeps its first and last
    /// `max_bytes / 2` bytes around an omission marker.
    pub fn read_full_output(&self, max_bytes: usize) -> FullOutput {
        let Some(path) = &self.temp_path else {
            return FullOutput { content: String::from_utf8_lossy(&self.raw_chunks).into_owned(), truncated: false };
        };
        read_file_output(path, max_bytes).unwrap_or_else(|_| FullOutput { content: String::new(), truncated: false })
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

    fn should_use_temp_file(&self) -> bool {
        self.total_raw_bytes > self.max_bytes
            || self.total_decoded_bytes > self.max_bytes
            || self.total_lines > self.max_lines
    }

    fn ensure_temp_file(&mut self) {
        if self.temp_path.is_some() {
            return;
        }
        let Some((path, mut file)) = create_output_file(self.temp_file_prefix, ".log") else {
            return;
        };
        let _ = file.write_all(&self.raw_chunks);
        self.raw_chunks = Vec::new();
        self.temp_path = Some(path);
        self.temp_file = Some(file);
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

/// Create `<tmp>/<prefix>-<random hex><ext>` readable only by the user.
fn create_output_file(prefix: &str, extension: &str) -> Option<(PathBuf, File)> {
    for _ in 0..8 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos());
        hasher.write_u128(nanos.unwrap_or_default());
        hasher.write_u32(std::process::id());
        let path = std::env::temp_dir().join(format!("{prefix}-{:016x}{extension}", hasher.finish()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        if let Ok(file) = options.open(&path) {
            return Some((path, file));
        }
    }
    None
}
