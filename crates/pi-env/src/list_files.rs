//! `list_files` through the daemon and `LocalExecutionEnv` (upstream `list-files.ts`).

use async_trait::async_trait;
use pi_chord::Context;
use pi_durable::env::{
    BinaryReader, DirReader, ExecutionError, FileError, FileErrorCode, FileInfo, FileKind, FileResult, FileSystem,
    FileWatcher, LocalEnvOptions, RemoveOptions, Shell, ShellCommand, ShellExecOptions, ShellExecResult,
    TempFileOptions, TextLineReader, WatchListener, WatchTarget,
};
use serde_json::Value;

use crate::connection::{Connection, Json, RequestOptions};
use crate::errors::{abort_result, to_file_error};

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// What `list_files` lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListFilesOptions {
    /// Include names starting with `.` (on Windows also files with the hidden attribute); default true. Entries named
    /// `.git` are never listed.
    pub hidden: Option<bool>,
    /// Inside a git work tree, skip what git ignores: the `.gitignore` files of the directory, its subdirectories and
    /// its parents, `.git/info/exclude`, and the global excludes file; default true. Outside work trees,
    /// `.gitignore` files have no effect.
    pub gitignore: Option<bool>,
    /// Names of entries to skip with everything below them, such as `node_modules`.
    pub exclude: Option<Vec<String>>,
    /// Most entries to list; default 50 000.
    pub max_entries: Option<u64>,
    /// Most bytes of the listing: each entry's UTF-8 path plus two; default 4 MiB, at most 15.9 MiB.
    pub max_bytes: Option<u64>,
    /// Time after which the listing stops; default 10 000 ms.
    pub timeout_ms: Option<u64>,
}

/// One entry of a `FileListing`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileListEntry {
    /// Relative to the listed directory, with `/` separators on every system.
    pub path: String,
    /// A symbolic link is listed as such and not followed.
    pub kind: FileKind,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileListing {
    /// Depth first, parents before their entries, siblings in byte order of their names.
    pub entries: Vec<FileListEntry>,
    /// A bound in `ListFilesOptions` ended the listing early.
    pub truncated: bool,
}

fn kind_of(byte: u8) -> Option<FileKind> {
    match byte {
        b'f' => Some(FileKind::File),
        b'd' => Some(FileKind::Directory),
        b'l' => Some(FileKind::Symlink),
        _ => None,
    }
}

/// Records of a kind byte, the UTF-8 path, and a NUL.
fn parse_listing(payload: &[u8]) -> Vec<FileListEntry> {
    let mut entries = Vec::new();
    let mut rest = payload;
    while let Some(end) = rest.iter().position(|byte| *byte == 0) {
        let record = &rest[..end];
        if let Some((kind, path)) = record.split_first().and_then(|(byte, path)| Some((kind_of(*byte)?, path))) {
            entries.push(FileListEntry { path: String::from_utf8_lossy(path).into_owned(), kind });
        }
        rest = &rest[end + 1..];
    }
    entries
}

/// `list_files` of an absolute path through the daemon: fails with `not_supported` if the daemon does not list files
/// (a release before `listFiles`), so callers fall back to their own walk.
pub async fn list_files_over(
    connection: &Connection,
    path: &str,
    options: Option<&ListFilesOptions>,
    context: &Context,
) -> FileResult<FileListing> {
    let options = options.cloned().unwrap_or_default();
    for (key, value) in
        [("maxEntries", options.max_entries), ("maxBytes", options.max_bytes), ("timeoutMs", options.timeout_ms)]
    {
        if value.is_some_and(|value| value > MAX_SAFE_INTEGER) {
            let message = format!("{key} must be a non-negative safe integer");
            return Err(FileError::new(FileErrorCode::Invalid, message, Some(path.into())));
        }
    }
    if let Some(aborted) = abort_result(context, Some(path)) {
        return aborted;
    }
    let info = connection.info().await.map_err(|error| to_file_error(&error, Some(path)))?;
    if !info.features.as_ref().is_some_and(|features| features.iter().any(|feature| feature == "listFiles")) {
        let message = "The pi-env daemon does not list files; update it";
        return Err(FileError::new(FileErrorCode::NotSupported, message, Some(path.into())));
    }
    // Fields left unset are not sent; the daemon applies the defaults.
    let mut json = Json::new();
    json.insert("path".into(), path.into());
    if let Some(hidden) = options.hidden {
        json.insert("hidden".into(), hidden.into());
    }
    if let Some(gitignore) = options.gitignore {
        json.insert("gitignore".into(), gitignore.into());
    }
    if let Some(exclude) = options.exclude {
        json.insert("exclude".into(), exclude.into());
    }
    for (key, value) in
        [("maxEntries", options.max_entries), ("maxBytes", options.max_bytes), ("timeoutMs", options.timeout_ms)]
    {
        if let Some(value) = value {
            json.insert(key.into(), value.into());
        }
    }
    let request = RequestOptions { signal: context.abort_signal(), ..Default::default() };
    let reply =
        connection.request("listFiles", json, request).await.map_err(|error| to_file_error(&error, Some(path)))?;
    if let Some(aborted) = abort_result(context, Some(path)) {
        return aborted;
    }
    let truncated = reply.json.get("truncated") == Some(&Value::Bool(true));
    Ok(FileListing { entries: parse_listing(&reply.payload), truncated })
}

/// `pi_durable::env::LocalExecutionEnv` (upstream `NodeExecutionEnv`) with `list_files` through a pi-env daemon on the
/// same machine, so local and remote environments list files with the same code. Everything else is the local
/// environment's. The connection belongs to the caller, who closes it; several environments may share it.
pub struct LocalExecutionEnv {
    env: pi_durable::env::LocalExecutionEnv,
    connection: Connection,
}

impl LocalExecutionEnv {
    /// `connection` is to this machine's daemon, whose binary `local_daemon` names.
    pub fn new(options: LocalEnvOptions, connection: Connection) -> Self {
        Self { env: pi_durable::env::LocalExecutionEnv::new(options), connection }
    }

    /// The local environment this one adds listing to.
    pub fn env(&self) -> &pi_durable::env::LocalExecutionEnv {
        &self.env
    }

    /// Through the daemon; `not_supported` with a daemon released before `listFiles`.
    pub async fn list_files(
        &self,
        path: &str,
        options: Option<&ListFilesOptions>,
        context: &Context,
    ) -> FileResult<FileListing> {
        let resolved = self.env.absolute_path(path, context).await?;
        list_files_over(&self.connection, &resolved, options, context).await
    }
}

#[async_trait]
impl FileSystem for LocalExecutionEnv {
    fn id(&self) -> &str {
        self.env.id()
    }

    fn cwd(&self) -> String {
        self.env.cwd()
    }

    async fn absolute_path(&self, path: &str, context: &Context) -> FileResult<String> {
        self.env.absolute_path(path, context).await
    }

    async fn join_path(&self, parts: &[String], context: &Context) -> FileResult<String> {
        self.env.join_path(parts, context).await
    }

    async fn read_text_file(&self, path: &str, context: &Context) -> FileResult<String> {
        self.env.read_text_file(path, context).await
    }

    async fn open_text_line_reader(&self, path: &str, context: &Context) -> FileResult<Box<dyn TextLineReader>> {
        self.env.open_text_line_reader(path, context).await
    }

    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        context: &Context,
    ) -> FileResult<Vec<String>> {
        self.env.read_text_lines(path, max_lines, context).await
    }

    async fn read_binary_file(&self, path: &str, context: &Context) -> FileResult<Vec<u8>> {
        self.env.read_binary_file(path, context).await
    }

    async fn open_binary_reader(
        &self,
        path: &str,
        no_follow: bool,
        context: &Context,
    ) -> FileResult<Box<dyn BinaryReader>> {
        self.env.open_binary_reader(path, no_follow, context).await
    }

    async fn write_file(&self, path: &str, content: &[u8], context: &Context) -> FileResult<()> {
        self.env.write_file(path, content, context).await
    }

    async fn append_file(&self, path: &str, content: &[u8], context: &Context) -> FileResult<()> {
        self.env.append_file(path, content, context).await
    }

    async fn truncate_file(&self, path: &str, size: u64, context: &Context) -> FileResult<()> {
        self.env.truncate_file(path, size, context).await
    }

    async fn flush_file(&self, path: &str, context: &Context) -> FileResult<()> {
        self.env.flush_file(path, context).await
    }

    async fn rename_file(&self, source: &str, destination: &str, context: &Context) -> FileResult<()> {
        self.env.rename_file(source, destination, context).await
    }

    async fn file_info(&self, path: &str, context: &Context) -> FileResult<FileInfo> {
        self.env.file_info(path, context).await
    }

    async fn list_dir(&self, path: &str, context: &Context) -> FileResult<Vec<FileInfo>> {
        self.env.list_dir(path, context).await
    }

    async fn open_dir_reader(&self, path: &str, context: &Context) -> FileResult<Box<dyn DirReader>> {
        self.env.open_dir_reader(path, context).await
    }

    async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: WatchListener,
        context: &Context,
    ) -> FileResult<Box<dyn FileWatcher>> {
        self.env.watch(targets, on_change, context).await
    }

    async fn canonical_path(&self, path: &str, context: &Context) -> FileResult<String> {
        self.env.canonical_path(path, context).await
    }

    async fn exists(&self, path: &str, context: &Context) -> FileResult<bool> {
        self.env.exists(path, context).await
    }

    async fn create_dir(&self, path: &str, recursive: bool, context: &Context) -> FileResult<()> {
        self.env.create_dir(path, recursive, context).await
    }

    async fn remove(&self, path: &str, options: RemoveOptions, context: &Context) -> FileResult<()> {
        self.env.remove(path, options, context).await
    }

    async fn create_temp_dir(&self, prefix: Option<&str>, context: &Context) -> FileResult<String> {
        self.env.create_temp_dir(prefix, context).await
    }

    async fn create_temp_file(&self, options: TempFileOptions, context: &Context) -> FileResult<String> {
        self.env.create_temp_file(options, context).await
    }

    async fn cleanup(&self, context: &Context) {
        self.env.cleanup(context).await
    }
}

#[async_trait]
impl Shell for LocalExecutionEnv {
    async fn exec(
        &self,
        command: ShellCommand,
        options: ShellExecOptions,
        context: &Context,
    ) -> Result<ShellExecResult, ExecutionError> {
        self.env.exec(command, options, context).await
    }
}
