//! `NodeExecutionEnv`'s mapping of Node error codes to `FileError` codes (upstream `errors.ts`).

use std::sync::Arc;

use pi_chord::Context;
use pi_durable::env::{FileError, FileErrorCode, FileResult};

use crate::connection::RemoteError;

/// `aborted` when the context is cancelled.
pub(crate) fn abort_result<T>(context: &Context, path: Option<&str>) -> Option<FileResult<T>> {
    context.is_cancelled().then(|| Err(aborted(path)))
}

pub(crate) fn aborted(path: Option<&str>) -> FileError {
    FileError::new(FileErrorCode::Aborted, "aborted", path.map(str::to_string))
}

/// The `FileError` of a daemon error; its path, else `fallback_path`.
pub fn to_file_error(error: &RemoteError, fallback_path: Option<&str>) -> FileError {
    let path = error.path.clone().or_else(|| fallback_path.map(str::to_string));
    let code = match error.code.as_str() {
        "aborted" => FileErrorCode::Aborted,
        "ENOENT" => FileErrorCode::NotFound,
        "EACCES" | "EPERM" => FileErrorCode::PermissionDenied,
        "ENOTDIR" => FileErrorCode::NotDirectory,
        "EISDIR" => FileErrorCode::IsDirectory,
        "EINVAL" | "SYMLINK" | "NOT_REGULAR" => FileErrorCode::Invalid,
        _ => FileErrorCode::Unknown,
    };
    FileError::new(code, error.message.clone(), path).with_cause(Arc::new(error.clone()))
}

/// A failure inside an operation: a daemon error, or a `FileError` the client raised itself.
#[derive(Debug)]
pub(crate) enum Failure {
    Remote(RemoteError),
    File(FileError),
}

impl From<RemoteError> for Failure {
    fn from(error: RemoteError) -> Self {
        Self::Remote(error)
    }
}

impl From<FileError> for Failure {
    fn from(error: FileError) -> Self {
        Self::File(error)
    }
}

impl Failure {
    pub fn into_file_error(self, fallback_path: Option<&str>) -> FileError {
        match self {
            Self::Remote(error) => to_file_error(&error, fallback_path),
            Self::File(error) => error,
        }
    }
}
