//! Errors that cross the boundary between JavaScript and Rust.

use std::fmt;
use std::io;

use serde::{Deserialize, Serialize};

/// Error code set on errors thrown by Node APIs that pi-rust does not implement.
pub const UNSUPPORTED_CODE: &str = "ERR_PI_UNSUPPORTED";

/// A JavaScript error seen from Rust, or a Rust error about to be thrown into JavaScript.
/// `stack` holds only the frames (`    at fn (file:line:col)`), mapped back to TypeScript
/// positions. `code` and `data` carry Node-style extras such as `ENOENT` or `path`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsError {
    pub name: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Extra enumerable properties copied onto the JavaScript error object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Box<serde_json::Map<String, serde_json::Value>>>,
}

impl JsError {
    pub fn new(message: impl Into<String>) -> Self {
        Self::with_name("Error", message)
    }

    pub fn with_name(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self { name: name.into(), message: message.into(), ..Self::default() }
    }

    pub fn type_error(message: impl Into<String>) -> Self {
        Self::with_name("TypeError", message)
    }

    pub fn range_error(message: impl Into<String>) -> Self {
        Self::with_name("RangeError", message)
    }

    /// The error thrown by Node APIs that pi-rust does not provide.
    pub fn unsupported(api: &str) -> Self {
        Self::new(format!("{api} is not supported in pi-rust")).with_code(UNSUPPORTED_CODE)
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn with_stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = Some(stack.into());
        self
    }

    /// Set one extra property copied onto the JavaScript error object.
    pub fn with_data(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        self.data.get_or_insert_with(Default::default).insert(key.to_string(), value.into());
        self
    }

    /// A Node-style system error: `ENOENT: no such file or directory, open '/x'`.
    pub fn from_io(error: &io::Error, syscall: &str, path: Option<&str>) -> Self {
        let (code, description) = io_code(error);
        let mut message = format!("{code}: {description}, {syscall}");
        if let Some(path) = path {
            message.push_str(&format!(" '{path}'"));
        }
        let mut result = Self::new(message).with_code(code).with_data("syscall", syscall);
        if let Some(errno) = error.raw_os_error() {
            result = result.with_data("errno", -errno);
        }
        if let Some(path) = path {
            result = result.with_data("path", path);
        }
        result
    }

    /// `Name: message` followed by the stack frames, the way Node prints an error.
    pub fn display_with_stack(&self) -> String {
        match self.stack.as_deref().map(str::trim_end).filter(|stack| !stack.is_empty()) {
            Some(stack) => format!("{self}\n{stack}"),
            None => self.to_string(),
        }
    }
}

impl fmt::Display for JsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.name.is_empty() {
            write!(f, "{}", self.message)
        } else if self.message.is_empty() {
            write!(f, "{}", self.name)
        } else {
            write!(f, "{}: {}", self.name, self.message)
        }
    }
}

impl std::error::Error for JsError {}

impl From<io::Error> for JsError {
    fn from(error: io::Error) -> Self {
        let (code, description) = io_code(&error);
        Self::new(format!("{code}: {description}")).with_code(code)
    }
}

impl From<serde_json::Error> for JsError {
    fn from(error: serde_json::Error) -> Self {
        Self::with_name("SyntaxError", error.to_string())
    }
}

/// The Node error code and its description for an I/O error.
pub(crate) fn io_code(error: &io::Error) -> (&'static str, &'static str) {
    use io::ErrorKind as K;
    if let Some(code) = error.raw_os_error().and_then(errno_code) {
        return code;
    }
    match error.kind() {
        K::NotFound => ("ENOENT", "no such file or directory"),
        K::PermissionDenied => ("EACCES", "permission denied"),
        K::AlreadyExists => ("EEXIST", "file already exists"),
        K::NotADirectory => ("ENOTDIR", "not a directory"),
        K::IsADirectory => ("EISDIR", "illegal operation on a directory"),
        K::DirectoryNotEmpty => ("ENOTEMPTY", "directory not empty"),
        K::InvalidInput => ("EINVAL", "invalid argument"),
        K::BrokenPipe => ("EPIPE", "broken pipe"),
        K::TimedOut => ("ETIMEDOUT", "operation timed out"),
        K::ResourceBusy => ("EBUSY", "resource busy or locked"),
        K::ReadOnlyFilesystem => ("EROFS", "read-only file system"),
        K::CrossesDevices => ("EXDEV", "cross-device link not permitted"),
        K::InvalidFilename => ("ENAMETOOLONG", "name too long"),
        K::ConnectionRefused => ("ECONNREFUSED", "connection refused"),
        K::ConnectionReset => ("ECONNRESET", "connection reset by peer"),
        K::Unsupported => ("ENOSYS", "function not implemented"),
        _ => ("EIO", "i/o error"),
    }
}

#[cfg(unix)]
fn errno_code(errno: i32) -> Option<(&'static str, &'static str)> {
    Some(match errno {
        1 => ("EPERM", "operation not permitted"),
        2 => ("ENOENT", "no such file or directory"),
        9 => ("EBADF", "bad file descriptor"),
        13 => ("EACCES", "permission denied"),
        16 => ("EBUSY", "resource busy or locked"),
        17 => ("EEXIST", "file already exists"),
        18 => ("EXDEV", "cross-device link not permitted"),
        20 => ("ENOTDIR", "not a directory"),
        21 => ("EISDIR", "illegal operation on a directory"),
        22 => ("EINVAL", "invalid argument"),
        24 => ("EMFILE", "too many open files"),
        28 => ("ENOSPC", "no space left on device"),
        30 => ("EROFS", "read-only file system"),
        32 => ("EPIPE", "broken pipe"),
        #[cfg(target_os = "linux")]
        36 => ("ENAMETOOLONG", "name too long"),
        #[cfg(target_os = "linux")]
        39 => ("ENOTEMPTY", "directory not empty"),
        #[cfg(target_os = "linux")]
        40 => ("ELOOP", "too many symbolic links encountered"),
        #[cfg(target_os = "macos")]
        62 => ("ELOOP", "too many symbolic links encountered"),
        #[cfg(target_os = "macos")]
        63 => ("ENAMETOOLONG", "name too long"),
        #[cfg(target_os = "macos")]
        66 => ("ENOTEMPTY", "directory not empty"),
        _ => return None,
    })
}

#[cfg(not(unix))]
fn errno_code(_errno: i32) -> Option<(&'static str, &'static str)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_errors_use_node_codes() {
        let error = io::Error::from(io::ErrorKind::NotFound);
        let mapped = JsError::from_io(&error, "open", Some("/missing"));
        assert_eq!(mapped.code.as_deref(), Some("ENOENT"));
        assert_eq!(mapped.message, "ENOENT: no such file or directory, open '/missing'");
        assert_eq!(mapped.data.unwrap()["path"], "/missing");
    }

    #[test]
    fn display_includes_name_and_stack() {
        let error = JsError::type_error("bad").with_stack("    at f (a.ts:1:2)");
        assert_eq!(error.to_string(), "TypeError: bad");
        assert_eq!(error.display_with_stack(), "TypeError: bad\n    at f (a.ts:1:2)");
        assert_eq!(JsError::unsupported("fs.watch").code.as_deref(), Some(UNSUPPORTED_CODE));
    }
}
