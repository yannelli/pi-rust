//! A [`ServerListener`] over one Unix-domain socket path.

use std::collections::HashMap;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pi_chord::{ChordError, ChordResult};
use pi_protocol::DEFAULT_MAX_FRAME_LENGTH;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use super::connection::UnixByteConnection;
use super::fs::{FileIdentity, cleanup_owned_socket, io_error, owned_bind_path, remove_path, remove_stale_socket};
use super::types::UnixListenerOptions;
use crate::connection::{ByteConnection, ByteConnectionAcceptor};
use crate::listener::ServerListener;
use crate::task::{SharedTask, lock, spawn_shared};
use crate::types::ErrorReporter;

const DEFAULT_SOCKET_MODE: u32 = 0o600;
const DEFAULT_GRACEFUL_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_TIMER_DELAY_MS: u128 = 2_147_483_647;

struct Resolved {
    path: PathBuf,
    mode: u32,
    graceful_close_timeout: Duration,
    max_pending_bytes: usize,
    on_error: Option<ErrorReporter>,
}

#[derive(Default)]
struct State {
    started: bool,
    closing: bool,
    accept_task: Option<JoinHandle<()>>,
    identity: Option<FileIdentity>,
    owned_bind_path: Option<PathBuf>,
    connections: HashMap<u64, Arc<UnixByteConnection>>,
}

struct Inner {
    options: Resolved,
    state: Mutex<State>,
    close_task: Mutex<Option<SharedTask<ChordResult<()>>>>,
    next_connection: AtomicU64,
}

struct UnixListener {
    inner: Arc<Inner>,
}

fn resolve(options: UnixListenerOptions) -> ChordResult<Resolved> {
    if options.path.as_os_str().is_empty() {
        return Err(ChordError::type_error("Server Unix socket path must not be empty"));
    }
    let mode = options.mode.unwrap_or(DEFAULT_SOCKET_MODE);
    if mode > 0o777 {
        return Err(ChordError::type_error("Server Unix socket mode must be an integer between 0 and 0o777"));
    }
    let max_frame_length = options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
    if max_frame_length == 0 {
        return Err(ChordError::type_error(format!(
            "Server maxFrameLength must be an integer between 1 and {}",
            u32::MAX
        )));
    }
    let max_frame_length = max_frame_length as usize;
    let max_pending_bytes = options.max_pending_bytes.unwrap_or(max_frame_length * 4);
    if max_pending_bytes < max_frame_length + 4 {
        return Err(ChordError::type_error(
            "Server maxPendingBytes must be a safe integer at least maxFrameLength + 4",
        ));
    }
    let graceful_close_timeout = options.graceful_close_timeout.unwrap_or(DEFAULT_GRACEFUL_CLOSE_TIMEOUT);
    if graceful_close_timeout.is_zero() || graceful_close_timeout.as_millis() > MAX_TIMER_DELAY_MS {
        return Err(ChordError::type_error(format!(
            "Server gracefulCloseTimeoutMs must be an integer between 1 and {MAX_TIMER_DELAY_MS}"
        )));
    }
    Ok(Resolved { path: options.path, mode, graceful_close_timeout, max_pending_bytes, on_error: options.on_error })
}

/// Create a listener for one Unix socket path. Options are validated immediately.
pub fn create_unix_listener(options: UnixListenerOptions) -> ChordResult<Arc<dyn ServerListener>> {
    let inner = Inner {
        options: resolve(options)?,
        state: Mutex::new(State::default()),
        close_task: Mutex::new(None),
        next_connection: AtomicU64::new(1),
    };
    Ok(Arc::new(UnixListener { inner: Arc::new(inner) }))
}

fn bind_and_publish(path: &Path, bind_path: &Path, mode: u32) -> ChordResult<(tokio::net::UnixListener, FileIdentity)> {
    let listener = std::os::unix::net::UnixListener::bind(bind_path).map_err(io_error)?;
    listener.set_nonblocking(true).map_err(io_error)?;
    let listener = tokio::net::UnixListener::from_std(listener).map_err(io_error)?;
    let identity = super::fs::socket_identity(bind_path)?.ok_or_else(|| {
        ChordError::error(format!("Unix listener path is not a socket after binding: {}", bind_path.display()))
    })?;
    std::fs::hard_link(bind_path, path).map_err(io_error)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(io_error)?;
    remove_path(bind_path)?;
    Ok((listener, identity))
}

impl Inner {
    fn report(&self, error: &ChordError) {
        if let Some(on_error) = &self.options.on_error {
            on_error(error);
        }
    }

    fn cleanup(&self) -> ChordResult<()> {
        let (bind_path, identity) = {
            let mut state = lock(&self.state);
            (state.owned_bind_path.take(), state.identity.take())
        };
        if let Some(bind_path) = bind_path {
            remove_path(&bind_path)?;
        }
        match identity {
            Some(identity) => cleanup_owned_socket(&self.options.path, identity),
            None => Ok(()),
        }
    }

    fn accept_socket(self: &Arc<Self>, stream: UnixStream, accept: &ByteConnectionAcceptor) {
        if lock(&self.state).closing {
            return;
        }
        let (mut reader, writer) = stream.into_split();
        let options = &self.options;
        let connection = UnixByteConnection::new(writer, options.graceful_close_timeout, options.max_pending_bytes);
        let id = self.next_connection.fetch_add(1, Ordering::Relaxed);
        lock(&self.state).connections.insert(id, connection.clone());
        let handler = accept(connection.clone() as Arc<dyn ByteConnection>);
        let inner = self.clone();
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let read = tokio::select! {
                    read = reader.read(&mut buffer) => read,
                    _ = connection.destroyed() => break,
                };
                match read {
                    Ok(0) => break,
                    Ok(count) => handler.on_data(&buffer[..count]),
                    Err(error) => {
                        handler.on_error(io_error(error));
                        break;
                    }
                }
            }
            connection.destroy();
            connection.mark_closed();
            lock(&inner.state).connections.remove(&id);
            handler.on_close();
        });
    }

    async fn close_internal(&self) -> ChordResult<()> {
        let (task, connections) = {
            let mut state = lock(&self.state);
            state.closing = true;
            (state.accept_task.take(), state.connections.values().cloned().collect::<Vec<_>>())
        };
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        let cleanup = self.cleanup();
        futures::future::join_all(connections.iter().map(|connection| connection.close(None))).await;
        lock(&self.state).connections.clear();
        cleanup
    }
}

#[async_trait]
impl ServerListener for UnixListener {
    async fn start(&self, accept: ByteConnectionAcceptor) -> ChordResult<()> {
        let inner = &self.inner;
        {
            let mut state = lock(&inner.state);
            if state.started {
                return Err(ChordError::error("Unix listener is already started"));
            }
            if state.closing {
                return Err(ChordError::error("Unix listener is closing or closed"));
            }
            state.started = true;
        }
        let path = inner.options.path.clone();
        let bind_path = owned_bind_path(&path);
        if let Some(parent) = path.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(io_error)?;
        }
        remove_stale_socket(&path).await?;
        remove_stale_socket(&bind_path).await?;
        lock(&inner.state).owned_bind_path = Some(bind_path.clone());
        let (listener, identity) = match bind_and_publish(&path, &bind_path, inner.options.mode) {
            Ok(bound) => bound,
            Err(error) => {
                let _ = inner.cleanup();
                return Err(error);
            }
        };
        {
            let mut state = lock(&inner.state);
            state.identity = Some(identity);
            state.owned_bind_path = None;
        }
        let task_inner = inner.clone();
        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => task_inner.accept_socket(stream, &accept),
                    Err(error) => {
                        task_inner.report(&io_error(error));
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        });
        lock(&inner.state).accept_task = Some(task);
        Ok(())
    }

    async fn close(&self) -> ChordResult<()> {
        let task = {
            let mut close_task = lock(&self.inner.close_task);
            let inner = self.inner.clone();
            close_task.get_or_insert_with(|| spawn_shared(async move { inner.close_internal().await })).clone()
        };
        task.await
    }
}
