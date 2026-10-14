//! Whole-file reads and writes, and directory listings, over daemon handles.

use std::collections::VecDeque;

use pi_chord::Context;
use pi_durable::env::{FileError, FileErrorCode, FileInfo, FileResult};
use serde_json::Value;

use super::readers::{Handle, InFlight, joined, read_range, to_info};
use super::{RemoteExecutionEnv, object, remote_path};
use crate::connection::{Json, RemoteError, RequestOptions};
use crate::errors::{Failure, aborted, to_file_error};
use crate::paths::PathStyle;

/// Node's `readFile`: chunk of known-size reads, chunk of unknown-size reads, and the largest file it reads.
const READ_FILE_CHUNK: u64 = 512 * 1024;
const READ_FILE_UNKNOWN_CHUNK: u64 = 64 * 1024;
const READ_FILE_MAX: u64 = (1 << 31) - 1;
/// Node's `writeFile` writes at most this much per call and checks for an abort before each.
const WRITE_CHUNK: usize = 512 * 1024;
const WRITE_DEPTH: usize = 8;

/// The bytes a `readdir` entry sorts by: its raw name bytes, else its UTF-8 name.
fn sort_key(entry: &Value) -> Vec<u8> {
    match entry.get("raw").and_then(Value::as_array) {
        Some(raw) => raw.iter().map(|byte| byte.as_u64().unwrap_or(0) as u8).collect(),
        None => entry.get("name").and_then(Value::as_str).unwrap_or("").as_bytes().to_vec(),
    }
}

impl RemoteExecutionEnv {
    /// Node's `readFile` over a file opened like `open(path, "r")`.
    pub(super) async fn read_file(&self, path: &str, context: &Context) -> FileResult<Vec<u8>> {
        self.file_op(path, context, false, |resolved| async move {
            let (handle, json) = self
                .open("open", object(&[("path", resolved.clone().into()), ("mode", "read".into())]), &resolved)
                .await?;
            let result = read_opened(&handle, &json, &resolved, context).await;
            handle.close().await;
            result
        })
        .await
    }

    /// Node's `writeFile` (abort checked before each 512 KiB write) or `appendFile` (no checks, one after): the first
    /// request creates parents, opens, and writes the first chunk; later chunks go to the same open file.
    pub(super) async fn write(&self, path: &str, content: &[u8], append: bool, context: &Context) -> FileResult<()> {
        self.file_op(path, context, append, |resolved| async move {
            let keep = content.len() > WRITE_CHUNK;
            let json = object(&[("path", resolved.clone().into()), ("append", append.into()), ("keep", keep.into())]);
            let first = content[..content.len().min(WRITE_CHUNK)].to_vec();
            let options = RequestOptions { payload: first, ..Default::default() };
            let reply = self.connection.request("write", json, options).await?;
            if !keep {
                return Ok(());
            }
            let id = reply.json.get("handle").and_then(Value::as_u64).unwrap_or(0);
            let style = remote_path(&self.connection).await?;
            let connection = self.connection.clone();
            let handle = Handle { connection, id, session: reply.session, path: resolved.clone(), style };
            let result = write_chunks(&handle, &content[WRITE_CHUNK..], !append, context).await;
            handle.close().await;
            result
        })
        .await
    }

    pub(super) async fn list(&self, resolved: String, context: &Context) -> Result<Vec<FileInfo>, Failure> {
        let (handle, _) = self.open("opendir", object(&[("path", resolved.clone().into())]), &resolved).await?;
        let mut entries: Vec<Value> = Vec::new();
        let listed = async {
            loop {
                let reply = handle.request("readdir", object(&[("max", 1000.into())]), Vec::new(), None).await?;
                if let Some(Value::Array(page)) = reply.get("entries") {
                    entries.extend(page.iter().cloned());
                }
                if reply.get("done") == Some(&Value::Bool(true)) {
                    return Ok::<_, RemoteError>(());
                }
            }
        }
        .await;
        handle.close().await;
        listed?;
        // Node's `readdir` fails on any entry it cannot lstat. libuv sorts names by their bytes on POSIX; on Windows
        // it keeps the file system's order, which the daemon reports.
        let style = handle.style;
        if style == PathStyle::Posix {
            entries.sort_by_cached_key(sort_key);
        }
        let mut infos = Vec::new();
        for entry in entries {
            if context.is_cancelled() {
                return Err(aborted(Some(&resolved)).into());
            }
            let entry_path = style.resolve(&resolved, entry.get("name").and_then(Value::as_str).unwrap_or(""));
            if let Some(Value::Object(error)) = entry.get("error") {
                return Err(to_file_error(&RemoteError::new(error.clone()), Some(&entry_path)).into());
            }
            if let Some(Value::Object(info)) = entry.get("info")
                && let Ok(info) = to_info(&entry_path, info, style)
            {
                infos.push(info);
            }
        }
        Ok(infos)
    }
}

async fn read_opened(handle: &Handle, json: &Json, resolved: &str, context: &Context) -> Result<Vec<u8>, Failure> {
    if let Some(Value::Object(error)) = json.get("statError") {
        return Err(RemoteError::new(error.clone()).into());
    }
    if context.is_cancelled() {
        return Err(aborted(Some(resolved)).into());
    }
    let stat = json.get("stat").and_then(Value::as_object);
    let regular = stat.and_then(|stat| stat.get("kind")).and_then(Value::as_str) == Some("file");
    let size = if regular { stat.and_then(|stat| stat.get("size")).and_then(Value::as_u64).unwrap_or(0) } else { 0 };
    if size > READ_FILE_MAX {
        let message = format!("File size ({size}) is greater than 2 GiB");
        return Err(FileError::new(FileErrorCode::Unknown, message, Some(resolved.into())).into());
    }
    if size == 0 {
        // Unknown size: sequential reads until the end, as for FIFOs and devices.
        let mut bytes = Vec::new();
        loop {
            if context.is_cancelled() {
                return Err(aborted(Some(resolved)).into());
            }
            let chunk = handle.pread(None, READ_FILE_UNKNOWN_CHUNK).await?;
            if chunk.is_empty() {
                return Ok(bytes);
            }
            bytes.extend_from_slice(&chunk);
        }
    }
    if size <= READ_FILE_CHUNK {
        return Ok(handle.pread(Some(0), size).await?);
    }
    match read_range(handle, 0, size, READ_FILE_CHUNK, || context.is_cancelled()).await? {
        Some(bytes) => Ok(bytes),
        None => Err(aborted(Some(resolved)).into()),
    }
}

/// Write `rest` as chunks to a kept write handle, several in flight; with `check`, abort between chunks.
async fn write_chunks(handle: &Handle, rest: &[u8], check: bool, context: &Context) -> Result<(), Failure> {
    let mut in_flight = InFlight(VecDeque::new());
    for chunk in rest.chunks(WRITE_CHUNK) {
        if check && context.is_cancelled() {
            // A chunk that failed before this check fails the write, as it would have in sequence.
            for (_, task) in std::mem::take(&mut in_flight.0) {
                joined(task.await)?;
            }
            return Err(aborted(Some(&handle.path)).into());
        }
        let (writer, payload) = (handle.clone(), chunk.to_vec());
        let task = tokio::spawn(async move { writer.request("writeChunk", Json::new(), payload, None).await });
        in_flight.0.push_back((0, task));
        if in_flight.0.len() >= WRITE_DEPTH
            && let Some((_, task)) = in_flight.0.pop_front()
        {
            joined(task.await)?;
        }
    }
    // The first failure; later chunks are refused by the daemon, so the file has no gaps.
    for (_, task) in std::mem::take(&mut in_flight.0) {
        joined(task.await)?;
    }
    Ok(())
}
