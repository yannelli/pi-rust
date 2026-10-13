//! Readers of the local environment: text lines, positional binary reads, and directory pages.

use std::fs::{File, ReadDir};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::Context;

use super::paths::{aborted, blocking, closed, file_info, io_error};
use crate::env::decode::StreamDecoder;
use crate::env::line_scan::LineScanner;
use crate::env::{
    BinaryReader, DirPage, DirReader, FileError, FileErrorCode, FileInfo, FileResult, LineRange, LineScan, TextLine,
    TextLineReader,
};

/// Largest single read of a `BinaryReader`, so a huge `length` allocates only as much as the file yields.
const BINARY_READ_CHUNK: u64 = 1024 * 1024;
const TEXT_CHUNK: usize = 64 * 1024;
pub(crate) const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Read at `offset` without moving a shared cursor; fewer bytes only at end of file.
fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    loop {
        #[cfg(unix)]
        let result = std::os::unix::fs::FileExt::read_at(file, buffer, offset);
        #[cfg(windows)]
        let result = std::os::windows::fs::FileExt::seek_read(file, buffer, offset);
        match result {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

struct TextState {
    file: Option<Arc<File>>,
    decoder: StreamDecoder,
    byte_offset: u64,
    buffered: String,
    /// Start of the unread text in `buffered`.
    consumed: usize,
    ended: bool,
}

/// Strict LF reader that reports whether each line was newline-terminated.
pub(crate) struct LocalTextLineReader {
    path: String,
    state: tokio::sync::Mutex<TextState>,
}

impl LocalTextLineReader {
    pub(crate) fn new(file: File, path: String) -> Self {
        let state = TextState {
            file: Some(Arc::new(file)),
            decoder: StreamDecoder::new(),
            byte_offset: 0,
            buffered: String::new(),
            consumed: 0,
            ended: false,
        };
        Self { path, state: tokio::sync::Mutex::new(state) }
    }
}

#[async_trait]
impl TextLineReader for LocalTextLineReader {
    async fn read_line(&self, context: &Context) -> FileResult<Option<TextLine>> {
        if context.is_cancelled() {
            return Err(aborted(&self.path));
        }
        let mut state = self.state.lock().await;
        let Some(file) = state.file.clone() else {
            return Err(FileError::new(FileErrorCode::Invalid, "Text line reader is closed", Some(self.path.clone())));
        };
        loop {
            let start = state.consumed;
            if let Some(newline) = state.buffered[start..].find('\n') {
                let text = state.buffered[start..start + newline].to_string();
                state.consumed = start + newline + 1;
                return Ok(Some(TextLine { text, terminated: true }));
            }
            if state.ended {
                if start == state.buffered.len() {
                    return Ok(None);
                }
                let text = state.buffered[start..].to_string();
                state.buffered.clear();
                state.consumed = 0;
                return Ok(Some(TextLine { text, terminated: false }));
            }
            // Explicit positions allow an aborted read to be retried without skipping bytes.
            let offset = state.byte_offset;
            let reading = file.clone();
            let read = blocking(move || {
                let mut chunk = vec![0u8; TEXT_CHUNK];
                read_at(&reading, &mut chunk, offset).map(|count| {
                    chunk.truncate(count);
                    chunk
                })
            })
            .await
            .map_err(|error| io_error(error, &self.path))?;
            if context.is_cancelled() {
                return Err(aborted(&self.path));
            }
            state.byte_offset += read.len() as u64;
            let text = if read.is_empty() {
                state.ended = true;
                state.decoder.finish()
            } else {
                state.decoder.decode(&read)
            };
            let consumed = state.consumed;
            state.buffered.drain(..consumed);
            state.consumed = 0;
            state.buffered.push_str(&text);
        }
    }

    async fn close(&self, _context: &Context) {
        let mut state = self.state.lock().await;
        state.file = None;
        state.buffered = String::new();
        state.consumed = 0;
    }
}

/// Positional reads from one opened regular file.
pub(crate) struct LocalBinaryReader {
    path: String,
    file: Mutex<Option<Arc<File>>>,
}

impl LocalBinaryReader {
    pub(crate) fn new(file: File, path: String) -> Self {
        Self { path, file: Mutex::new(Some(Arc::new(file))) }
    }

    fn open_file(&self, context: &Context) -> FileResult<Arc<File>> {
        if context.is_cancelled() {
            return Err(aborted(&self.path));
        }
        let file = self.file.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        file.ok_or_else(|| closed("Binary reader", &self.path))
    }
}

#[async_trait]
impl BinaryReader for LocalBinaryReader {
    async fn info(&self, context: &Context) -> FileResult<FileInfo> {
        let file = self.open_file(context)?;
        let path = self.path.clone();
        blocking(move || {
            file.metadata().map_err(|error| io_error(error, &path)).and_then(|meta| file_info(&path, &meta))
        })
        .await
    }

    async fn read(&self, offset: u64, length: u64, context: &Context) -> FileResult<Vec<u8>> {
        let file = self.open_file(context)?;
        if offset > MAX_SAFE_INTEGER || length > MAX_SAFE_INTEGER {
            let message = "Offset and length must be non-negative safe integers";
            return Err(FileError::new(FileErrorCode::Invalid, message, Some(self.path.clone())));
        }
        let (path, context) = (self.path.clone(), context.clone());
        blocking(move || {
            let mut bytes = Vec::new();
            while (bytes.len() as u64) < length {
                let total = bytes.len();
                let size = (length - total as u64).min(BINARY_READ_CHUNK) as usize;
                bytes.resize(total + size, 0);
                let count = read_at(&file, &mut bytes[total..], offset + total as u64)
                    .map_err(|error| io_error(error, &path))?;
                bytes.truncate(total + count);
                if context.is_cancelled() {
                    return Err(aborted(&path));
                }
                if count == 0 {
                    break;
                }
            }
            Ok(bytes)
        })
        .await
    }

    async fn scan_lines(&self, range: LineRange, context: &Context) -> FileResult<LineScan> {
        let file = self.open_file(context)?;
        let Ok(mut scanner) = LineScanner::new(range.start_line, range.end_line) else {
            return Err(FileError::new(FileErrorCode::Invalid, "Invalid line range", Some(self.path.clone())));
        };
        let (path, context) = (self.path.clone(), context.clone());
        blocking(move || {
            let mut chunk = vec![0u8; TEXT_CHUNK];
            let mut position = 0u64;
            loop {
                let count = read_at(&file, &mut chunk, position).map_err(|error| io_error(error, &path))?;
                if context.is_cancelled() {
                    return Err(aborted(&path));
                }
                if count == 0 {
                    return Ok(scanner.finish());
                }
                scanner.push(&chunk[..count]);
                position += count as u64;
            }
        })
        .await
    }

    async fn close(&self, _context: &Context) {
        self.file.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
    }
}

struct DirState {
    entries: Option<ReadDir>,
    done: bool,
}

/// Pages of one directory's entries.
pub(crate) struct LocalDirReader {
    path: String,
    state: Arc<Mutex<DirState>>,
}

impl LocalDirReader {
    pub(crate) fn new(entries: ReadDir, path: String) -> Self {
        Self { path, state: Arc::new(Mutex::new(DirState { entries: Some(entries), done: false })) }
    }
}

#[async_trait]
impl DirReader for LocalDirReader {
    async fn next(&self, max_entries: usize, context: &Context) -> FileResult<DirPage> {
        if context.is_cancelled() {
            return Err(aborted(&self.path));
        }
        let (state, path, context) = (self.state.clone(), self.path.clone(), context.clone());
        blocking(move || {
            let mut state = state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.entries.is_none() {
                return Err(closed("Directory reader", &path));
            }
            if max_entries == 0 || max_entries as u64 > MAX_SAFE_INTEGER {
                let message = "maxEntries must be a positive safe integer";
                return Err(FileError::new(FileErrorCode::Invalid, message, Some(path)));
            }
            let mut entries = Vec::new();
            while !state.done && entries.len() < max_entries {
                let Some(entry) = state.entries.as_mut().and_then(Iterator::next) else {
                    state.done = true;
                    break;
                };
                let entry = entry.map_err(|error| io_error(error, &path))?;
                if context.is_cancelled() {
                    return Err(aborted(&path));
                }
                let entry_path = Path::new(&path).join(entry.file_name()).to_string_lossy().into_owned();
                match std::fs::symlink_metadata(&entry_path) {
                    // Removed between enumeration and lstat: not part of the listing any more.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(io_error(error, &entry_path)),
                    Ok(metadata) => {
                        if let Ok(info) = file_info(&entry_path, &metadata) {
                            entries.push(info);
                        }
                    }
                }
            }
            Ok(DirPage { entries, done: state.done })
        })
        .await
    }

    async fn close(&self, _context: &Context) {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).entries = None;
    }
}
