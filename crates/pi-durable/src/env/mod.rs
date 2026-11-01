//! The execution environment: a portable file system and shell that tools, sections, and hosts use.
//!
//! Operations return failures as values (`FileError`, `ExecutionError`) rather than panicking.

pub mod decode;
pub mod line_scan;
pub mod local;
mod types;

use std::sync::Arc;

use async_trait::async_trait;
use pi_chord::Context;

pub use decode::{StreamDecoder, starts_with_bom};
pub use line_scan::{InvalidLineRange, LineScanner};
pub use local::{LocalEnvOptions, LocalExecutionEnv, LocalFileWatcher, LocalWatchOptions};
pub use types::*;

/// Result of a file operation.
pub type FileResult<T> = Result<T, FileError>;

/// Lines of one opened text file, read one at a time.
#[async_trait]
pub trait TextLineReader: Send + Sync {
    /// The next line, or `None` at the end of the file.
    async fn read_line(&self, context: &Context) -> FileResult<Option<TextLine>>;
    async fn close(&self, context: &Context);
}

/// Positional reads from one opened regular file; all calls see the same file even if its path is renamed.
#[async_trait]
pub trait BinaryReader: Send + Sync {
    /// Metadata of the opened file, not of whatever its path names now.
    async fn info(&self, context: &Context) -> FileResult<FileInfo>;
    /// Up to `length` bytes at `offset`; fewer only at end of file.
    async fn read(&self, offset: u64, length: u64, context: &Context) -> FileResult<Vec<u8>>;
    /// One pass over the file that finds the selected lines, where line `k` starts after the `k`-th newline byte.
    /// Decoded sizes do not count a byte-order mark at the start of the file.
    async fn scan_lines(&self, range: LineRange, context: &Context) -> FileResult<LineScan>;
    async fn close(&self, context: &Context);
}

/// Pages of one directory's entries.
#[async_trait]
pub trait DirReader: Send + Sync {
    /// Up to `max_entries` entries in file system order, continuing where the last call stopped. Entries that
    /// disappear before their metadata is read are skipped. After a failed or aborted call, close the reader.
    async fn next(&self, max_entries: usize, context: &Context) -> FileResult<DirPage>;
    async fn close(&self, context: &Context);
}

/// A running watch.
#[async_trait]
pub trait FileWatcher: Send + Sync {
    fn mode(&self) -> WatchMode;
    /// Stop watching; no listener call starts after this returns. Idempotent.
    async fn stop(&self, context: &Context);
}

/// Portable file system capability.
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// The file namespace: equal ids see the same files at the same paths, whatever their `cwd`. Every local
    /// environment shares one id; each container or remote host has its own.
    fn id(&self) -> &str;
    fn cwd(&self) -> String;
    async fn absolute_path(&self, path: &str, context: &Context) -> FileResult<String>;
    async fn join_path(&self, parts: &[String], context: &Context) -> FileResult<String>;
    async fn read_text_file(&self, path: &str, context: &Context) -> FileResult<String>;
    async fn open_text_line_reader(&self, path: &str, context: &Context) -> FileResult<Box<dyn TextLineReader>>;
    async fn read_text_lines(&self, path: &str, max_lines: Option<usize>, context: &Context)
    -> FileResult<Vec<String>>;
    async fn read_binary_file(&self, path: &str, context: &Context) -> FileResult<Vec<u8>>;
    /// Open a regular file for bounded positional reads. A directory fails with `is_directory`, other non-regular
    /// files with `invalid`. With `no_follow`, a symbolic link as the final component fails with `invalid`.
    async fn open_binary_reader(
        &self,
        path: &str,
        no_follow: bool,
        context: &Context,
    ) -> FileResult<Box<dyn BinaryReader>>;
    /// Write a whole file; text is passed as its UTF-8 bytes.
    async fn write_file(&self, path: &str, content: &[u8], context: &Context) -> FileResult<()>;
    async fn append_file(&self, path: &str, content: &[u8], context: &Context) -> FileResult<()>;
    /// Truncate or extend a file to exactly `size` bytes.
    async fn truncate_file(&self, path: &str, size: u64, context: &Context) -> FileResult<()>;
    /// Flush file contents and the metadata needed to retrieve them.
    async fn flush_file(&self, path: &str, context: &Context) -> FileResult<()>;
    async fn rename_file(&self, source: &str, destination: &str, context: &Context) -> FileResult<()>;
    async fn file_info(&self, path: &str, context: &Context) -> FileResult<FileInfo>;
    async fn list_dir(&self, path: &str, context: &Context) -> FileResult<Vec<FileInfo>>;
    async fn open_dir_reader(&self, path: &str, context: &Context) -> FileResult<Box<dyn DirReader>>;
    /// Optional: the files, directories and symbolic links below a directory, at any depth, for indexes such as file
    /// completion. Symbolic links are not followed; other kinds of files are not listed; entries that cannot be read
    /// are skipped. See `ListFilesOptions` for hidden and ignored files and for the bounds. Fails with `not_supported`
    /// when the environment cannot list right now (an older remote daemon), so callers fall back to their own walk.
    /// The default is `not_supported`.
    async fn list_files(
        &self,
        path: &str,
        _options: Option<&ListFilesOptions>,
        _context: &Context,
    ) -> FileResult<FileListing> {
        Err(FileError::new(FileErrorCode::NotSupported, "This environment does not list files", Some(path.into())))
    }
    /// Report changes to files and directories. Once the watcher exists, coverage is established: a host that watches
    /// before it loads cannot miss a change made during the load.
    async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: WatchListener,
        context: &Context,
    ) -> FileResult<Box<dyn FileWatcher>>;
    async fn canonical_path(&self, path: &str, context: &Context) -> FileResult<String>;
    async fn exists(&self, path: &str, context: &Context) -> FileResult<bool>;
    async fn create_dir(&self, path: &str, recursive: bool, context: &Context) -> FileResult<()>;
    async fn remove(&self, path: &str, options: RemoveOptions, context: &Context) -> FileResult<()>;
    async fn create_temp_dir(&self, prefix: Option<&str>, context: &Context) -> FileResult<String>;
    async fn create_temp_file(&self, options: TempFileOptions, context: &Context) -> FileResult<String>;
    /// Shut the environment down, for its owner, never for one request. An environment with a shell also kills
    /// every command it still runs.
    async fn cleanup(&self, context: &Context);
}

/// Command execution capability.
#[async_trait]
pub trait Shell: Send + Sync {
    /// Run a command. Aborting the context or a timeout kills only this command's processes.
    async fn exec(
        &self,
        command: ShellCommand,
        options: ShellExecOptions,
        context: &Context,
    ) -> Result<ShellExecResult, ExecutionError>;
}

/// A file system and a shell over the same files.
pub trait ExecutionEnv: FileSystem + Shell {}

impl<T: FileSystem + Shell + ?Sized> ExecutionEnv for T {}

/// Shared handle of an execution environment.
pub type SharedEnv = Arc<dyn ExecutionEnv>;
