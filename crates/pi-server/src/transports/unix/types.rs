//! Unix transport options.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::types::ErrorReporter;

#[derive(Clone, Default)]
pub struct UnixListenerOptions {
    pub path: PathBuf,
    /// Socket filesystem permissions. Defaults to owner read/write only (0o600).
    pub mode: Option<u32>,
    /// Maximum framed bytes queued per connection before a slow peer is disconnected.
    pub max_pending_bytes: Option<usize>,
    pub graceful_close_timeout: Option<Duration>,
    /// Used to derive and validate `max_pending_bytes`. Must match the server when customized.
    pub max_frame_length: Option<u32>,
    pub on_error: Option<ErrorReporter>,
}

impl UnixListenerOptions {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), ..Self::default() }
    }
}

/// Server options plus the listener options of its single Unix socket.
#[derive(Clone, Default)]
pub struct UnixServerOptions {
    pub server_id: String,
    pub path: PathBuf,
    pub mode: Option<u32>,
    pub max_frame_length: Option<u32>,
    pub max_pending_bytes: Option<usize>,
    pub graceful_close_timeout: Option<Duration>,
    pub handshake_timeout: Option<Duration>,
    pub on_connection_count_changed: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    pub on_error: Option<ErrorReporter>,
}

impl UnixServerOptions {
    pub fn new(server_id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self { server_id: server_id.into(), path: path.into(), ..Self::default() }
    }
}
