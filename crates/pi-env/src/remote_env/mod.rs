//! `RemoteExecutionEnv` (upstream `remote-env.ts`): an `ExecutionEnv` on another machine, reached through a pi-env
//! daemon. The daemon makes the system calls; this client applies Node's path, error, and result rules.

mod exec;
mod io;
mod readers;

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::Context;
use pi_durable::env::{
    BinaryReader, DirReader, FileError, FileErrorCode, FileInfo, FileListing, FileResult, FileSystem, FileWatcher,
    ListFilesOptions, RemoveOptions, TempFileOptions, TextLineReader, WatchListener, WatchTarget,
};
use serde_json::Value;

use crate::connection::{Connection, Json, RemoteError, RequestOptions, lock};
use crate::errors::{Failure, abort_result, to_file_error};
use crate::list_files::list_files_over;
use crate::paths::{PathStyle, file_url_to_path};
use crate::session::random_hex;
use crate::watch::{RemoteWatchOptions, RemoteWatcher};
use readers::{Handle, RemoteBinaryReader, RemoteDirReader, RemoteTextLineReader, to_info};

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Options of `RemoteExecutionEnv::new`.
#[derive(Clone)]
pub struct RemoteExecutionEnvOptions {
    pub connection: Connection,
    /// The file namespace: equal ids see the same files (`FileSystem::id`), e.g. `pi-env:<env name>`.
    pub id: String,
    pub cwd: String,
    pub shell_path: Option<String>,
    /// Added to the remote environment of every command that inherits it, like the local env's `shell_env`.
    pub shell_env: Option<BTreeMap<String, String>>,
    /// How the daemon watches, like the local env's `watch` option.
    pub watch: RemoteWatchOptions,
}

impl RemoteExecutionEnvOptions {
    pub fn new(connection: Connection, id: impl Into<String>, cwd: impl Into<String>) -> Self {
        Self {
            connection,
            id: id.into(),
            cwd: cwd.into(),
            shell_path: None,
            shell_env: None,
            watch: RemoteWatchOptions::default(),
        }
    }
}

/// An `ExecutionEnv` on another machine. Results match the local environment running on that machine.
pub struct RemoteExecutionEnv {
    id: String,
    cwd: String,
    pub connection: Connection,
    shell_path: Option<String>,
    shell_env: Option<BTreeMap<String, String>>,
    watch_options: RemoteWatchOptions,
    /// Running commands this environment started (request id to session), for `cleanup()`.
    running: Arc<Mutex<HashMap<u32, u64>>>,
}

/// Node's path rules of the remote system.
pub async fn remote_path(connection: &Connection) -> Result<PathStyle, RemoteError> {
    let info = connection.info().await?;
    Ok(PathStyle::of_info(&info))
}

fn object(pairs: &[(&str, Value)]) -> Json {
    pairs.iter().map(|(key, value)| (key.to_string(), value.clone())).collect()
}

impl RemoteExecutionEnv {
    pub fn new(options: RemoteExecutionEnvOptions) -> Self {
        Self {
            id: options.id,
            cwd: options.cwd,
            connection: options.connection,
            shell_path: options.shell_path,
            shell_env: options.shell_env,
            watch_options: options.watch,
            running: Arc::default(),
        }
    }

    /// Node's `resolvePath` on the remote system, with its home directory and working directories.
    async fn resolve(&self, path: &str) -> Result<String, RemoteError> {
        let info = self.connection.info().await?;
        let style = PathStyle::of_info(&info);
        let windows = style == PathStyle::Windows;
        let mut normalized = path.to_string();
        if path == "~" {
            normalized = info.home.clone();
        } else if path.starts_with("~/") || (windows && path.starts_with("~\\")) {
            normalized = style.join(&[&info.home, &path[2..]]);
        } else if path.starts_with("file://")
            && let Some(file) = file_url_to_path(path, style)
        {
            normalized = file;
        }
        if style.is_absolute(&normalized) {
            return Ok(style.resolve(&self.cwd, &normalized));
        }
        let bytes = normalized.as_bytes();
        let drive_relative = bytes.len() >= 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes.get(2).is_none_or(|c| *c != b'\\' && *c != b'/');
        if windows && drive_relative && !normalized[..2].eq_ignore_ascii_case(self.cwd.get(..2).unwrap_or("")) {
            // A drive-relative path on another drive: that drive's working directory (`=D:`), else the daemon's
            // working directory if on that drive, else the drive's root.
            let drive = &normalized[..2];
            let mut base = info.drive_cwds.get(&drive.to_ascii_uppercase()).cloned().unwrap_or(info.cwd.clone());
            if !base.get(..2).unwrap_or("").eq_ignore_ascii_case(drive) && base.as_bytes().get(2) == Some(&b'\\') {
                base = format!("{drive}\\");
            }
            return Ok(style.resolve(&base, &normalized));
        }
        Ok(style.resolve(&self.cwd, &normalized))
    }

    /// Resolve `path`, check for an abort, and run `operation`; with `check_after`, check again after it.
    async fn file_op<'a, T, F, Fut>(&'a self, path: &str, context: &Context, check_after: bool, run: F) -> FileResult<T>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<T, Failure>> + 'a,
    {
        let resolved = match self.resolve(path).await {
            Ok(resolved) => resolved,
            Err(error) => return Err(to_file_error(&error, Some(path))),
        };
        if let Some(aborted) = abort_result(context, Some(&resolved)) {
            return aborted;
        }
        match run(resolved.clone()).await {
            Ok(value) => match check_after.then(|| abort_result(context, Some(&resolved))).flatten() {
                Some(aborted) => aborted,
                None => Ok(value),
            },
            Err(failure) => Err(failure.into_file_error(Some(&resolved))),
        }
    }

    async fn call(&self, op: &str, json: Json) -> Result<Json, RemoteError> {
        Ok(self.connection.request(op, json, RequestOptions::default()).await?.json)
    }

    async fn open(&self, op: &str, json: Json, path: &str) -> Result<(Handle, Json), RemoteError> {
        let reply = self.connection.request(op, json, RequestOptions::default()).await?;
        let style = remote_path(&self.connection).await?;
        let id = reply.json.get("handle").and_then(Value::as_u64).unwrap_or(0);
        let handle =
            Handle { connection: self.connection.clone(), id, session: reply.session, path: path.into(), style };
        Ok((handle, reply.json))
    }

    /// Open a reader; when the context aborted meanwhile, close it again.
    async fn open_reader<R>(
        &self,
        path: &str,
        json: Json,
        op: &str,
        context: &Context,
        make: impl FnOnce(Handle) -> R,
    ) -> FileResult<R> {
        let handle = self
            .file_op(path, context, false, |resolved| async move {
                let mut json = json;
                json.insert("path".into(), resolved.clone().into());
                Ok(self.open(op, json, &resolved).await?.0)
            })
            .await?;
        if let Some(aborted) = abort_result(context, Some(&handle.path)) {
            handle.close().await;
            return aborted;
        }
        Ok(make(handle))
    }
}

#[async_trait]
impl FileSystem for RemoteExecutionEnv {
    fn id(&self) -> &str {
        &self.id
    }

    fn cwd(&self) -> String {
        self.cwd.clone()
    }

    async fn absolute_path(&self, path: &str, _context: &Context) -> FileResult<String> {
        self.resolve(path).await.map_err(|error| to_file_error(&error, Some(path)))
    }

    async fn join_path(&self, parts: &[String], _context: &Context) -> FileResult<String> {
        let style = remote_path(&self.connection).await.map_err(|error| to_file_error(&error, None))?;
        Ok(style.join(&parts.iter().map(String::as_str).collect::<Vec<_>>()))
    }

    async fn read_text_file(&self, path: &str, context: &Context) -> FileResult<String> {
        Ok(String::from_utf8_lossy(&self.read_file(path, context).await?).into_owned())
    }

    async fn open_text_line_reader(&self, path: &str, context: &Context) -> FileResult<Box<dyn TextLineReader>> {
        let json = object(&[("mode", "read".into())]);
        self.open_reader(path, json, "open", context, |handle| -> Box<dyn TextLineReader> {
            Box::new(RemoteTextLineReader { handle, state: Default::default(), closed: AtomicBool::new(false) })
        })
        .await
    }

    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        context: &Context,
    ) -> FileResult<Vec<String>> {
        if max_lines == Some(0) {
            return Ok(Vec::new());
        }
        let reader = self.open_text_line_reader(path, context).await?;
        let mut lines = Vec::new();
        let result = loop {
            if max_lines.is_some_and(|max| lines.len() >= max) {
                break Ok(());
            }
            match reader.read_line(context).await {
                Ok(Some(line)) => lines.push(line.text),
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        reader.close(context).await;
        result.map(|()| lines)
    }

    async fn read_binary_file(&self, path: &str, context: &Context) -> FileResult<Vec<u8>> {
        self.read_file(path, context).await
    }

    async fn open_binary_reader(
        &self,
        path: &str,
        no_follow: bool,
        context: &Context,
    ) -> FileResult<Box<dyn BinaryReader>> {
        let json = object(&[("noFollow", no_follow.into())]);
        self.open_reader(path, json, "open", context, |handle| -> Box<dyn BinaryReader> {
            Box::new(RemoteBinaryReader { handle, closed: AtomicBool::new(false) })
        })
        .await
    }

    async fn write_file(&self, path: &str, content: &[u8], context: &Context) -> FileResult<()> {
        self.write(path, content, false, context).await
    }

    async fn append_file(&self, path: &str, content: &[u8], context: &Context) -> FileResult<()> {
        self.write(path, content, true, context).await
    }

    async fn truncate_file(&self, path: &str, size: u64, context: &Context) -> FileResult<()> {
        self.file_op(path, context, true, |resolved| async move {
            if size > MAX_SAFE_INTEGER {
                let message = "File size must be a non-negative safe integer";
                return Err(FileError::new(FileErrorCode::Invalid, message, Some(resolved)).into());
            }
            self.call("truncate", object(&[("path", resolved.into()), ("size", size.into())])).await?;
            Ok(())
        })
        .await
    }

    async fn flush_file(&self, path: &str, context: &Context) -> FileResult<()> {
        self.file_op(path, context, true, |resolved| async move {
            self.call("fsync", object(&[("path", resolved.into())])).await?;
            Ok(())
        })
        .await
    }

    async fn rename_file(&self, source: &str, destination: &str, context: &Context) -> FileResult<()> {
        let resolved = async { Ok::<_, RemoteError>((self.resolve(source).await?, self.resolve(destination).await?)) };
        let (from, to) = resolved.await.map_err(|error| to_file_error(&error, Some(source)))?;
        if let Some(aborted) = abort_result(context, Some(&to)) {
            return aborted;
        }
        let json = object(&[("path", from.clone().into()), ("to", to.into())]);
        self.call("rename", json).await.map(|_| ()).map_err(|error| to_file_error(&error, Some(&from)))
    }

    async fn file_info(&self, path: &str, context: &Context) -> FileResult<FileInfo> {
        self.file_op(path, context, false, |resolved| async move {
            let json = self.call("lstat", object(&[("path", resolved.clone().into())])).await?;
            Ok(to_info(&resolved, &json, remote_path(&self.connection).await?)?)
        })
        .await
    }

    async fn list_dir(&self, path: &str, context: &Context) -> FileResult<Vec<FileInfo>> {
        self.file_op(path, context, false, |resolved| self.list(resolved, context)).await
    }

    async fn open_dir_reader(&self, path: &str, context: &Context) -> FileResult<Box<dyn DirReader>> {
        self.open_reader(path, Json::new(), "opendir", context, |handle| -> Box<dyn DirReader> {
            Box::new(RemoteDirReader { handle, done: AtomicBool::new(false), closed: AtomicBool::new(false) })
        })
        .await
    }

    /// Through the daemon; `not_supported` with a daemon released before `listFiles`.
    async fn list_files(
        &self,
        path: &str,
        options: Option<&ListFilesOptions>,
        context: &Context,
    ) -> FileResult<FileListing> {
        let resolved = self.resolve(path).await.map_err(|error| to_file_error(&error, Some(path)))?;
        list_files_over(&self.connection, &resolved, options, context).await
    }

    async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: WatchListener,
        context: &Context,
    ) -> FileResult<Box<dyn FileWatcher>> {
        if let Some(aborted) = abort_result(context, None) {
            return aborted;
        }
        let mut resolved = Vec::with_capacity(targets.len());
        for target in targets {
            let path = self.resolve(&target.path).await.map_err(|error| to_file_error(&error, None))?;
            resolved.push(WatchTarget { path, ..target.clone() });
        }
        if let Some(aborted) = abort_result(context, None) {
            return aborted;
        }
        let watcher = RemoteWatcher::open(self.connection.clone(), resolved, on_change, self.watch_options.clone())
            .await
            .map_err(|error| to_file_error(&error, None))?;
        if let Some(aborted) = abort_result(context, None) {
            watcher.close(context).await;
            return aborted;
        }
        Ok(Box::new(watcher))
    }

    async fn canonical_path(&self, path: &str, context: &Context) -> FileResult<String> {
        self.file_op(path, context, false, |resolved| async move {
            let json = self.call("realpath", object(&[("path", resolved.into())])).await?;
            Ok(json.get("path").and_then(Value::as_str).unwrap_or_default().to_string())
        })
        .await
    }

    async fn exists(&self, path: &str, context: &Context) -> FileResult<bool> {
        match self.file_info(path, context).await {
            Ok(_) => Ok(true),
            Err(error) if error.code == FileErrorCode::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn create_dir(&self, path: &str, recursive: bool, context: &Context) -> FileResult<()> {
        self.file_op(path, context, false, |resolved| async move {
            self.call("mkdir", object(&[("path", resolved.into()), ("recursive", recursive.into())])).await?;
            Ok(())
        })
        .await
    }

    async fn remove(&self, path: &str, options: RemoveOptions, context: &Context) -> FileResult<()> {
        self.file_op(path, context, false, |resolved| async move {
            let json = object(&[
                ("path", resolved.into()),
                ("recursive", options.recursive.into()),
                ("force", options.force.into()),
            ]);
            self.call("rm", json).await?;
            Ok(())
        })
        .await
    }

    async fn create_temp_dir(&self, prefix: Option<&str>, context: &Context) -> FileResult<String> {
        if let Some(aborted) = abort_result(context, None) {
            return aborted;
        }
        let made = async {
            let info = self.connection.info().await?;
            let path = PathStyle::of_info(&info).join(&[&info.tmpdir, prefix.unwrap_or("tmp-")]);
            self.call("mkdtemp", object(&[("path", path.into())])).await
        };
        let json = made.await.map_err(|error| to_file_error(&error, None))?;
        Ok(json.get("path").and_then(Value::as_str).unwrap_or_default().to_string())
    }

    async fn create_temp_file(&self, options: TempFileOptions, context: &Context) -> FileResult<String> {
        let dir = self.create_temp_dir(Some("tmp-"), context).await?;
        let style = remote_path(&self.connection).await.map_err(|error| to_file_error(&error, None))?;
        let (prefix, suffix) = (options.prefix.unwrap_or_default(), options.suffix.unwrap_or_default());
        let file = style.join(&[&dir, &format!("{prefix}{}{suffix}", uuid_v4())]);
        // Node's `writeFile(filePath, "")`, which does not create parents.
        let json = object(&[("path", file.clone().into()), ("append", false.into()), ("parents", false.into())]);
        self.call("write", json).await.map_err(|error| to_file_error(&error, Some(&file)))?;
        Ok(file)
    }

    async fn cleanup(&self, _context: &Context) {
        // Kill without aborting, so the commands settle with their killed status, as the local env does.
        let running: Vec<(u32, u64)> = lock(&self.running).drain().collect();
        for (id, session) in running {
            self.connection.kill(id, session);
        }
    }
}

/// A random version 4 UUID, as `crypto.randomUUID()` formats it.
fn uuid_v4() -> String {
    let mut hex: Vec<char> = random_hex(16).chars().collect();
    hex[12] = '4';
    hex[16] = ['8', '9', 'a', 'b'][hex[16].to_digit(16).unwrap_or(0) as usize % 4];
    let hex: String = hex.into_iter().collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}
