//! Value types of the execution environment: errors, file metadata, line scans, watches, and shell options.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use pi_chord::Context;
use serde::{Deserialize, Serialize};

/// Kind of a directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

/// Why a file operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileErrorCode {
    Aborted,
    NotFound,
    PermissionDenied,
    NotDirectory,
    IsDirectory,
    Invalid,
    NotSupported,
    Unknown,
}

impl FileErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::NotFound => "not_found",
            Self::PermissionDenied => "permission_denied",
            Self::NotDirectory => "not_directory",
            Self::IsDirectory => "is_directory",
            Self::Invalid => "invalid",
            Self::NotSupported => "not_supported",
            Self::Unknown => "unknown",
        }
    }
}

/// Source error kept as the cause of an environment failure.
pub type ErrorCause = Arc<dyn std::error::Error + Send + Sync>;

/// A failed file operation. Operations return it rather than panicking.
#[derive(Clone)]
pub struct FileError {
    pub code: FileErrorCode,
    pub message: String,
    pub path: Option<String>,
    pub cause: Option<ErrorCause>,
}

impl FileError {
    pub fn new(code: FileErrorCode, message: impl Into<String>, path: Option<String>) -> Self {
        Self { code, message: message.into(), path, cause: None }
    }

    pub fn with_cause(mut self, cause: ErrorCause) -> Self {
        self.cause = Some(cause);
        self
    }

    /// The error of an aborted operation: `aborted`, "The operation was aborted".
    pub fn aborted(path: Option<String>) -> Self {
        Self::new(FileErrorCode::Aborted, "The operation was aborted", path)
    }
}

impl fmt::Debug for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileError")
            .field("code", &self.code)
            .field("message", &self.message)
            .field("path", &self.path)
            .finish()
    }
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FileError {}

/// Why a shell command failed to run to an exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionErrorCode {
    Aborted,
    Timeout,
    ShellUnavailable,
    SpawnError,
    CallbackError,
    Unknown,
}

impl ExecutionErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::Timeout => "timeout",
            Self::ShellUnavailable => "shell_unavailable",
            Self::SpawnError => "spawn_error",
            Self::CallbackError => "callback_error",
            Self::Unknown => "unknown",
        }
    }
}

/// A failed command execution.
#[derive(Clone)]
pub struct ExecutionError {
    pub code: ExecutionErrorCode,
    pub message: String,
    /// Spill file of a command that timed out or was aborted after its output crossed the spill thresholds.
    pub spill_path: Option<String>,
    pub cause: Option<ErrorCause>,
}

impl ExecutionError {
    pub fn new(code: ExecutionErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), spill_path: None, cause: None }
    }

    pub fn with_cause(mut self, cause: ErrorCause) -> Self {
        self.cause = Some(cause);
        self
    }
}

impl fmt::Debug for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionError")
            .field("code", &self.code)
            .field("message", &self.message)
            .field("spill_path", &self.spill_path)
            .finish()
    }
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExecutionError {}

/// Metadata of one file or directory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub name: String,
    pub path: String,
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ms: f64,
}

/// One line of a text file and whether a newline ended it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextLine {
    pub text: String,
    pub terminated: bool,
}

/// Lines `[start_line, end_line)` to find, 0-based; `end_line` absent: to the end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineRange {
    pub start_line: u64,
    pub end_line: Option<u64>,
}

/// What `FileSystem::list_files` lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListFilesOptions {
    /// Include names starting with `.` (on Windows also files with the hidden attribute); default true. Entries named
    /// `.git` are never listed.
    pub hidden: Option<bool>,
    /// Inside a git work tree, skip what git ignores: the `.gitignore` files of the directory, its subdirectories and
    /// its parents, `.git/info/exclude`, and the global excludes file; default true. Unlike `git ls-files`, a tracked
    /// file that matches an ignore rule is skipped too. Outside work trees, `.gitignore` files have no effect.
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

/// Where lines of a file are, as `BinaryReader::scan_lines` found them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineScan {
    /// Newline bytes in the whole file; it has `newlines + 1` lines.
    pub newlines: u64,
    /// Byte range of the selected lines, without the newline that ends the last one. Empty past the last line.
    pub start: u64,
    pub end: u64,
    /// Where the first selected line ends: its newline, or the end of the file.
    pub first_line_end: u64,
    /// Where the last selected line starts.
    pub last_line_start: u64,
    /// UTF-8 byte length of the decoded selection and of its first line.
    pub selected_bytes: u64,
    pub first_line_bytes: u64,
}

/// Entries of a directory below a watched path that are neither watched nor reported.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatchExclude {
    /// Names starting with `.`.
    pub hidden: bool,
    pub names: Vec<String>,
}

/// A file or directory to watch. It may be missing; creating it is a change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatchTarget {
    pub path: String,
    /// Watch everything below a directory, not only its entries. Symbolic links below it are not followed.
    pub recursive: bool,
    pub exclude: Option<WatchExclude>,
}

/// What changed below the watched targets.
#[derive(Clone, Debug)]
pub enum WatchChange {
    /// Something at or below each path may have changed. Calls may be spurious; none is missed while healthy.
    Paths(Vec<String>),
    /// Coverage was uncertain for a while (lost events, reconnect); rescan everything that is watched.
    Overflow,
    /// The watcher stopped, for example because the tree grew past the environment's limit; no calls follow.
    Error(FileError),
}

/// How a watcher learns about changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchMode {
    /// Changes are reported within about two seconds.
    Native,
    /// Snapshots are compared; a change undone between two snapshots can be missed.
    Polling,
}

/// Receives watch changes.
pub type WatchListener = Arc<dyn Fn(WatchChange) + Send + Sync>;

/// Options of `FileSystem::remove`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemoveOptions {
    pub recursive: bool,
    pub force: bool,
}

/// Options of `FileSystem::create_temp_file`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TempFileOptions {
    pub prefix: Option<String>,
    pub suffix: Option<String>,
}

/// One page of a directory read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DirPage {
    pub entries: Vec<FileInfo>,
    /// The end of the directory; it may come with the last entries or with an empty page.
    pub done: bool,
}

/// A command to run: through the environment's shell, or as a program and its arguments without a shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellCommand {
    Script(String),
    Argv(Vec<String>),
}

impl From<&str> for ShellCommand {
    fn from(value: &str) -> Self {
        Self::Script(value.to_string())
    }
}

impl From<String> for ShellCommand {
    fn from(value: String) -> Self {
        Self::Script(value)
    }
}

impl From<Vec<String>> for ShellCommand {
    fn from(value: Vec<String>) -> Self {
        Self::Argv(value)
    }
}

/// Spill the complete output to a temporary file once it exceeds either threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellSpillOptions {
    pub after_bytes: u64,
    /// Complete or partial lines.
    pub after_lines: u64,
}

/// The tail of the combined output a caller keeps, and how often it samples it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShellOutputWindow {
    /// UTF-8 bytes of decoded text kept at the end of the output.
    pub max_bytes: u64,
    /// Lines kept at the end of the output.
    pub max_lines: u64,
    /// Minimum pause between the caller's samples of the output.
    pub min_interval_ms: f64,
    /// Each sample also pauses the caller in proportion to its size at this rate.
    pub bytes_per_second: f64,
}

/// Output an environment omitted, measured on the decoded text the callback would have received.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellOutputSkip {
    /// UTF-8 byte length of the omitted text; every U+FFFD counts as three bytes.
    pub bytes: u64,
    /// Newlines (U+000A) in the omitted text.
    pub newlines: u64,
    pub ends_with_newline: bool,
}

/// Which stream an output chunk came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Facts about one output chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellOutputInfo {
    pub stream: OutputStream,
    /// Output omitted immediately before this chunk; only with a window.
    pub skipped: Option<ShellOutputSkip>,
}

/// Receives every decoded chunk of stdout and stderr as it arrives, with the stream it came from.
pub type OutputCallback = Arc<dyn Fn(&str, &Context, &ShellOutputInfo) + Send + Sync>;

/// Options of `Shell::exec`.
#[derive(Clone, Default)]
pub struct ShellExecOptions {
    pub cwd: Option<String>,
    pub env: Option<BTreeMap<String, String>>,
    pub inherit_env: Option<bool>,
    /// Seconds.
    pub timeout: Option<f64>,
    pub on_output: Option<OutputCallback>,
    pub spill: Option<ShellSpillOptions>,
    /// The caller keeps only this tail, so the environment may omit output outside it.
    pub window: Option<ShellOutputWindow>,
}

impl fmt::Debug for ShellExecOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShellExecOptions")
            .field("cwd", &self.cwd)
            .field("env", &self.env)
            .field("inherit_env", &self.inherit_env)
            .field("timeout", &self.timeout)
            .field("on_output", &self.on_output.is_some())
            .field("spill", &self.spill)
            .field("window", &self.window)
            .finish()
    }
}

/// The exit of a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellExecResult {
    pub exit_code: i32,
    /// Temporary file holding the complete raw output, when the spill thresholds were exceeded.
    pub spill_path: Option<String>,
}
