//! Path resolution, metadata, and error mapping of the local environment.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::env::{FileError, FileErrorCode, FileInfo, FileKind, FileResult};

pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Lexically normalize a path like Node's `path.normalize`: `.` dropped, `..` applied, and the trailing separator gone.
/// A relative path keeps leading `..`; an empty result is `.`.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut parts: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match parts.last() {
                Some(Component::Normal(_)) => {
                    parts.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => parts.push(component),
            },
            other => parts.push(other),
        }
    }
    let normalized: PathBuf = parts.iter().collect();
    if normalized.as_os_str().is_empty() { PathBuf::from(".") } else { normalized }
}

fn file_url_path(url: &str) -> Option<PathBuf> {
    url::Url::parse(url).ok()?.to_file_path().ok()
}

/// Resolve `path` against `cwd` like the upstream environment: `~` and `~/` expand to the home directory, and a
/// `file://` URL becomes its path. Malformed URLs stay ordinary paths.
pub(crate) fn resolve_path(cwd: &str, path: &str) -> String {
    let mut normalized = PathBuf::from(path);
    if path == "~" {
        normalized = home_dir();
    } else if let Some(rest) = path.strip_prefix("~/") {
        normalized = home_dir().join(rest);
    } else if cfg!(windows) && path.starts_with("~\\") {
        normalized = home_dir().join(&path[2..]);
    } else if path.starts_with("file://") {
        if let Some(file) = file_url_path(path) {
            normalized = file;
        }
    }
    let joined = if normalized.is_absolute() { normalized } else { Path::new(cwd).join(normalized) };
    normalize(&joined).to_string_lossy().into_owned()
}

/// Node's `path.join`: the parts joined with the separator, then normalized.
pub(crate) fn join_path(parts: &[String]) -> String {
    let joined: Vec<&str> = parts.iter().map(String::as_str).filter(|part| !part.is_empty()).collect();
    if joined.is_empty() {
        return ".".to_string();
    }
    let separator = std::path::MAIN_SEPARATOR_STR;
    normalize(Path::new(&joined.join(separator))).to_string_lossy().into_owned()
}

pub(crate) fn base_name(path: &str) -> String {
    Path::new(path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

pub(crate) fn file_kind(file_type: std::fs::FileType) -> Option<FileKind> {
    if file_type.is_symlink() {
        Some(FileKind::Symlink)
    } else if file_type.is_file() {
        Some(FileKind::File)
    } else if file_type.is_dir() {
        Some(FileKind::Directory)
    } else {
        None
    }
}

pub(crate) fn mtime_ms(metadata: &std::fs::Metadata) -> f64 {
    match metadata.modified() {
        Ok(time) => match time.duration_since(UNIX_EPOCH) {
            Ok(after) => after.as_secs_f64() * 1000.0,
            Err(before) => -(before.duration().as_secs_f64() * 1000.0),
        },
        Err(_) => 0.0,
    }
}

/// `FileInfo` of `path` from its metadata; other kinds than files, directories, and links are `invalid`.
pub(crate) fn file_info(path: &str, metadata: &std::fs::Metadata) -> FileResult<FileInfo> {
    let Some(kind) = file_kind(metadata.file_type()) else {
        return Err(FileError::new(FileErrorCode::Invalid, "Unsupported file type", Some(path.to_string())));
    };
    Ok(FileInfo {
        name: base_name(path),
        path: path.to_string(),
        kind,
        size: metadata.len(),
        mtime_ms: mtime_ms(metadata),
    })
}

/// The error code of an I/O error, as upstream maps Node's error codes.
pub(crate) fn error_code(error: &io::Error) -> FileErrorCode {
    match error.kind() {
        io::ErrorKind::NotFound => FileErrorCode::NotFound,
        io::ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
        io::ErrorKind::NotADirectory => FileErrorCode::NotDirectory,
        io::ErrorKind::IsADirectory => FileErrorCode::IsDirectory,
        io::ErrorKind::InvalidInput => FileErrorCode::Invalid,
        _ => raw_error_code(error),
    }
}

#[cfg(unix)]
fn raw_error_code(error: &io::Error) -> FileErrorCode {
    use rustix::io::Errno;
    match error.raw_os_error().map(Errno::from_raw_os_error) {
        Some(Errno::PERM) => FileErrorCode::PermissionDenied,
        Some(Errno::INVAL) => FileErrorCode::Invalid,
        _ => FileErrorCode::Unknown,
    }
}

#[cfg(not(unix))]
fn raw_error_code(_error: &io::Error) -> FileErrorCode {
    FileErrorCode::Unknown
}

/// A `FileError` for an I/O failure at `path`, keeping the I/O error as its cause.
pub(crate) fn io_error(error: io::Error, path: &str) -> FileError {
    let code = error_code(&error);
    let message = error.to_string();
    FileError::new(code, message, Some(path.to_string())).with_cause(Arc::new(error))
}

pub(crate) fn aborted(path: &str) -> FileError {
    FileError::new(FileErrorCode::Aborted, "aborted", Some(path.to_string()))
}

pub(crate) fn closed(what: &str, path: &str) -> FileError {
    FileError::new(FileErrorCode::Invalid, format!("{what} is closed"), Some(path.to_string()))
}

/// Run blocking file work on tokio's blocking pool.
pub(crate) async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => value,
        Err(error) => std::panic::resume_unwind(error.into_panic()),
    }
}
