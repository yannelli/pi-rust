//! Daemon handles and the readers over them: binary reader, directory reader, and text line reader.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use pi_chord::{AbortSignal, Context};
use pi_durable::env::{
    BinaryReader, DirPage, DirReader, FileError, FileErrorCode, FileInfo, FileKind, FileResult, LineRange, LineScan,
    StreamDecoder, TextLine, TextLineReader,
};
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::connection::{Connection, Json, RemoteError, RequestOptions};
use crate::errors::{abort_result, aborted, to_file_error};
use crate::paths::PathStyle;

/// Bytes per `pread` request of a binary reader; several are in flight at once.
pub(crate) const READ_CHUNK: u64 = 256 * 1024;
const READ_DEPTH: usize = 8;
/// `NodeTextLineReader`'s read size.
const LINE_CHUNK: u64 = 64 * 1024;
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// A `FileInfo` from the daemon's `info`; `other` file kinds are `invalid`.
pub(crate) fn to_info(path: &str, remote: &Json, style: PathStyle) -> FileResult<FileInfo> {
    let kind = match remote.get("kind").and_then(Value::as_str) {
        Some("file") => FileKind::File,
        Some("directory") => FileKind::Directory,
        Some("symlink") => FileKind::Symlink,
        _ => return Err(FileError::new(FileErrorCode::Invalid, "Unsupported file type", Some(path.into()))),
    };
    let number = |key: &str| remote.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    Ok(FileInfo {
        name: style.basename(path),
        path: path.to_string(),
        kind,
        size: remote.get("size").and_then(Value::as_u64).unwrap_or(0),
        mtime_ms: number("mtimeSec") * 1000.0 + number("mtimeNsec") / 1e6,
    })
}

/// A daemon handle: valid only in the session that opened it.
#[derive(Clone)]
pub(crate) struct Handle {
    pub connection: Connection,
    pub id: u64,
    pub session: u64,
    pub path: String,
    pub style: PathStyle,
}

impl Handle {
    pub async fn request(
        &self,
        op: &str,
        mut json: Json,
        payload: Vec<u8>,
        signal: Option<AbortSignal>,
    ) -> Result<Json, RemoteError> {
        json.insert("handle".into(), self.id.into());
        let options = RequestOptions { payload, signal, session: Some(self.session), ..Default::default() };
        Ok(self.connection.request(op, json, options).await?.json)
    }

    pub async fn pread(&self, offset: Option<u64>, length: u64) -> Result<Vec<u8>, RemoteError> {
        self.pread_now(offset, length).await
    }

    /// Send a `pread` now; the future waits for its bytes.
    pub fn pread_now(
        &self,
        offset: Option<u64>,
        length: u64,
    ) -> impl Future<Output = Result<Vec<u8>, RemoteError>> + Send + use<> {
        let mut json = Json::new();
        json.insert("handle".into(), self.id.into());
        if let Some(offset) = offset {
            json.insert("offset".into(), offset.into());
        }
        json.insert("length".into(), length.into());
        let reply = self.connection.request_now("pread", json, self.session, RequestOptions::default());
        async move { Ok(reply.await?.payload) }
    }

    /// Send a request with a payload now, so chunks reach the daemon in order.
    pub fn send_now(
        &self,
        op: &str,
        payload: Vec<u8>,
    ) -> impl Future<Output = Result<Json, RemoteError>> + Send + use<> {
        let mut json = Json::new();
        json.insert("handle".into(), self.id.into());
        let options = RequestOptions { payload, ..Default::default() };
        let reply = self.connection.request_now(op, json, self.session, options);
        async move { Ok(reply.await?.json) }
    }

    pub async fn close(&self) {
        let _ = self.request("close", Json::new(), Vec::new(), None).await;
    }
}

/// Spawned requests that are aborted when dropped.
pub(crate) struct InFlight<T: Send + 'static>(pub VecDeque<(u64, JoinHandle<T>)>);

impl<T: Send + 'static> Drop for InFlight<T> {
    fn drop(&mut self) {
        for (_, task) in &self.0 {
            task.abort();
        }
    }
}

pub(crate) fn joined<T>(result: Result<Result<T, RemoteError>, tokio::task::JoinError>) -> Result<T, RemoteError> {
    result.unwrap_or_else(|error| Err(RemoteError::lost(&error.to_string())))
}

/// Read `length` bytes from `offset` as consecutive reads of at most `chunk` bytes, several in flight. Ends early at
/// the end of the file; a short read continues from where it ended. `None` means `aborted()` held after a read.
pub(crate) async fn read_range(
    handle: &Handle,
    offset: u64,
    length: u64,
    chunk: u64,
    aborted: impl Fn() -> bool,
) -> Result<Option<Vec<u8>>, RemoteError> {
    let mut bytes = Vec::new();
    let mut next = offset;
    let mut in_flight = InFlight(VecDeque::new());
    while (bytes.len() as u64) < length {
        while in_flight.0.len() < READ_DEPTH && next < offset + length {
            let size = (offset + length - next).min(chunk);
            in_flight.0.push_back((size, tokio::spawn(handle.pread_now(Some(next), size))));
            next += size;
        }
        let Some((size, task)) = in_flight.0.pop_front() else { break };
        let read = joined(task.await)?;
        if aborted() {
            return Ok(None);
        }
        if read.is_empty() {
            break;
        }
        bytes.extend_from_slice(&read);
        if (read.len() as u64) < size {
            // Reads already in flight assumed a full read; continue right after this one.
            for (_, task) in in_flight.0.drain(..) {
                task.abort();
            }
            next = offset + bytes.len() as u64;
        }
    }
    Ok(Some(bytes))
}

pub(crate) struct RemoteBinaryReader {
    pub handle: Handle,
    pub closed: AtomicBool,
}

impl RemoteBinaryReader {
    fn early<T>(&self, context: &Context) -> Option<FileResult<T>> {
        let path = &self.handle.path;
        abort_result(context, Some(path)).or_else(|| {
            self.closed
                .load(Ordering::SeqCst)
                .then(|| Err(FileError::new(FileErrorCode::Invalid, "Binary reader is closed", Some(path.clone()))))
        })
    }
}

#[async_trait]
impl BinaryReader for RemoteBinaryReader {
    async fn info(&self, context: &Context) -> FileResult<FileInfo> {
        if let Some(early) = self.early(context) {
            return early;
        }
        let path = &self.handle.path;
        match self.handle.request("fstat", Json::new(), Vec::new(), None).await {
            Ok(json) => to_info(path, &json, self.handle.style),
            Err(error) => Err(to_file_error(&error, Some(path))),
        }
    }

    async fn read(&self, offset: u64, length: u64, context: &Context) -> FileResult<Vec<u8>> {
        if let Some(early) = self.early(context) {
            return early;
        }
        let path = &self.handle.path;
        if offset > MAX_SAFE_INTEGER || length > MAX_SAFE_INTEGER {
            let message = "Offset and length must be non-negative safe integers";
            return Err(FileError::new(FileErrorCode::Invalid, message, Some(path.clone())));
        }
        match read_range(&self.handle, offset, length, READ_CHUNK, || context.is_cancelled()).await {
            Ok(Some(bytes)) => Ok(bytes),
            Ok(None) => Err(aborted(Some(path))),
            Err(error) => Err(to_file_error(&error, Some(path))),
        }
    }

    async fn scan_lines(&self, range: LineRange, context: &Context) -> FileResult<LineScan> {
        if let Some(early) = self.early(context) {
            return early;
        }
        let path = &self.handle.path;
        if range.start_line > MAX_SAFE_INTEGER || range.end_line.is_some_and(|end| end <= range.start_line) {
            return Err(FileError::new(FileErrorCode::Invalid, "Invalid line range", Some(path.clone())));
        }
        let mut json = Json::new();
        json.insert("startLine".into(), range.start_line.into());
        if let Some(end) = range.end_line {
            json.insert("endLine".into(), end.into());
        }
        match self.handle.request("scanLines", json, Vec::new(), context.abort_signal()).await {
            Ok(json) => serde_json::from_value(Value::Object(json)).map_err(|error| {
                FileError::new(FileErrorCode::Unknown, format!("Invalid line scan: {error}"), Some(path.clone()))
            }),
            Err(error) => Err(to_file_error(&error, Some(path))),
        }
    }

    async fn close(&self, _context: &Context) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.handle.close().await;
        }
    }
}

pub(crate) struct RemoteDirReader {
    pub handle: Handle,
    pub done: AtomicBool,
    pub closed: AtomicBool,
}

#[async_trait]
impl DirReader for RemoteDirReader {
    async fn next(&self, max_entries: usize, context: &Context) -> FileResult<DirPage> {
        let path = &self.handle.path;
        if let Some(aborted) = abort_result(context, Some(path)) {
            return aborted;
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(FileError::new(FileErrorCode::Invalid, "Directory reader is closed", Some(path.clone())));
        }
        if max_entries == 0 {
            let message = "maxEntries must be a positive safe integer";
            return Err(FileError::new(FileErrorCode::Invalid, message, Some(path.clone())));
        }
        let style = self.handle.style;
        let mut entries = Vec::new();
        // Like `NodeDirReader`, count only entries that are part of the listing.
        while !self.done.load(Ordering::SeqCst) && entries.len() < max_entries {
            let mut json = Json::new();
            json.insert("max".into(), (max_entries - entries.len()).into());
            let reply = match self.handle.request("readdir", json, Vec::new(), None).await {
                Ok(reply) => reply,
                Err(error) => return Err(to_file_error(&error, Some(path))),
            };
            if let Some(aborted) = abort_result(context, Some(path)) {
                return aborted;
            }
            for entry in reply.get("entries").and_then(Value::as_array).into_iter().flatten() {
                let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
                let entry_path = style.resolve(path, name);
                if let Some(Value::Object(error)) = entry.get("error") {
                    // Removed between enumeration and lstat: not part of the listing any more.
                    if error.get("code").and_then(Value::as_str) == Some("ENOENT") {
                        continue;
                    }
                    return Err(to_file_error(&RemoteError::new(error.clone()), Some(&entry_path)));
                }
                if let Some(Value::Object(info)) = entry.get("info")
                    && let Ok(info) = to_info(&entry_path, info, style)
                {
                    entries.push(info);
                }
            }
            self.done.store(reply.get("done") == Some(&Value::Bool(true)), Ordering::SeqCst);
        }
        Ok(DirPage { entries, done: self.done.load(Ordering::SeqCst) })
    }

    async fn close(&self, _context: &Context) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.handle.close().await;
        }
    }
}

#[derive(Default)]
pub(crate) struct LineState {
    decoder: StreamDecoder,
    offset: u64,
    buffered: String,
    ended: bool,
}

/// `NodeTextLineReader`: positional reads of a file opened like `open(path, "r")`.
pub(crate) struct RemoteTextLineReader {
    pub handle: Handle,
    pub state: tokio::sync::Mutex<LineState>,
    pub closed: AtomicBool,
}

#[async_trait]
impl TextLineReader for RemoteTextLineReader {
    async fn read_line(&self, context: &Context) -> FileResult<Option<TextLine>> {
        let path = &self.handle.path;
        if let Some(aborted) = abort_result(context, Some(path)) {
            return aborted;
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(FileError::new(FileErrorCode::Invalid, "Text line reader is closed", Some(path.clone())));
        }
        let mut state = self.state.lock().await;
        loop {
            if let Some(newline) = state.buffered.find('\n') {
                let text = state.buffered[..newline].to_string();
                state.buffered.drain(..=newline);
                return Ok(Some(TextLine { text, terminated: true }));
            }
            if state.ended {
                if state.buffered.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(TextLine { text: std::mem::take(&mut state.buffered), terminated: false }));
            }
            let bytes = match self.handle.pread(Some(state.offset), LINE_CHUNK).await {
                Ok(bytes) => bytes,
                Err(error) => return Err(to_file_error(&error, Some(path))),
            };
            if let Some(aborted) = abort_result(context, Some(path)) {
                return aborted;
            }
            state.offset += bytes.len() as u64;
            let text = if bytes.is_empty() {
                state.ended = true;
                state.decoder.finish()
            } else {
                state.decoder.decode(&bytes)
            };
            state.buffered.push_str(&text);
        }
    }

    async fn close(&self, _context: &Context) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.state.lock().await.buffered.clear();
            self.handle.close().await;
        }
    }
}
