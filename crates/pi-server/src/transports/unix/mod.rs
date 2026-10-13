//! Unix-domain socket transport: listener, server preset, and socket-path derivation.

mod connection;
mod fs;
mod listener;
mod types;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_chord::{ChordError, ChordResult};
use pi_protocol::is_server_id;

pub use connection::UnixByteConnection;
pub use listener::create_unix_listener;
pub use types::{UnixListenerOptions, UnixServerOptions};

use crate::server::Server;
use crate::types::{ServerHost, ServerOptions};

/// Derive the local Unix socket path for one logical server identity.
pub fn get_unix_socket_path(server_id: &str, server_directory: impl AsRef<Path>) -> ChordResult<PathBuf> {
    if !is_server_id(server_id) {
        return Err(ChordError::type_error("Unix serverId must be a canonical lowercase UUIDv4"));
    }
    Ok(server_directory.as_ref().join(format!("{server_id}.sock")))
}

/// Compose a [`Server`] with one Unix-domain socket listener.
pub fn create_unix_server<H: ServerHost>(host: Arc<H>, options: UnixServerOptions) -> ChordResult<Server<H>> {
    let listener = create_unix_listener(UnixListenerOptions {
        path: options.path,
        mode: options.mode,
        max_pending_bytes: options.max_pending_bytes,
        graceful_close_timeout: options.graceful_close_timeout,
        max_frame_length: options.max_frame_length,
        on_error: options.on_error.clone(),
    })?;
    Server::new(
        host,
        ServerOptions {
            listeners: vec![listener],
            server_id: options.server_id,
            max_frame_length: options.max_frame_length,
            handshake_timeout: options.handshake_timeout,
            on_connection_count_changed: options.on_connection_count_changed,
            on_error: options.on_error,
        },
    )
}
