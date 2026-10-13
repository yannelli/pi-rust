//! The protocol server: listener composition, handshakes, and request routing.

mod connection;
mod requests;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use pi_chord::{ChordError, ChordResult, Context};
use pi_protocol::{DEFAULT_MAX_FRAME_LENGTH, is_server_id};
use tokio::sync::watch;

use crate::connection::{ByteConnection, ByteConnectionAcceptor, ByteConnectionHandler};
use crate::listener::ServerListener;
use crate::session_router::{RouterOptions, SessionRouter};
use crate::task::{SharedTask, lock, spawn_shared};
use crate::types::{ErrorReporter, ServerHost, ServerOptions};
use connection::Conn;

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_TIMER_DELAY_MS: u128 = 2_147_483_647;

#[derive(Default)]
struct Lifecycle {
    starting: Option<SharedTask<ChordResult<()>>>,
    started: bool,
    close: Option<SharedTask<ChordResult<()>>>,
}

pub(crate) struct ServerInner<H: ServerHost> {
    host: Arc<H>,
    listeners: Vec<Arc<dyn ServerListener>>,
    server_id: String,
    max_frame_length: u32,
    handshake_timeout: Duration,
    on_connection_count_changed: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    on_error: Option<ErrorReporter>,
    connections: Mutex<HashMap<u64, Arc<Conn>>>,
    next_connection: AtomicU64,
    sessions: Arc<SessionRouter<H>>,
    closing: Arc<AtomicBool>,
    lifecycle: Mutex<Lifecycle>,
    closed: watch::Sender<Option<ChordResult<()>>>,
    me: Weak<ServerInner<H>>,
}

/// Routes clients to application-hosted durable Sessions over any set of listeners.
pub struct Server<H: ServerHost> {
    inner: Arc<ServerInner<H>>,
}

impl<H: ServerHost> Clone for Server<H> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

fn resolve_options(options: &ServerOptions) -> ChordResult<(u32, Duration)> {
    if !is_server_id(&options.server_id) {
        return Err(ChordError::type_error("serverId must be a canonical lowercase UUIDv4"));
    }
    let max_frame_length = options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
    if max_frame_length == 0 {
        return Err(ChordError::type_error(format!(
            "Server maxFrameLength must be an integer between 1 and {}",
            u32::MAX
        )));
    }
    let handshake_timeout = options.handshake_timeout.unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT);
    if handshake_timeout.is_zero() || handshake_timeout.as_millis() > MAX_TIMER_DELAY_MS {
        return Err(ChordError::type_error(format!(
            "Server handshakeTimeoutMs must be an integer between 1 and {MAX_TIMER_DELAY_MS}"
        )));
    }
    Ok((max_frame_length, handshake_timeout))
}

impl<H: ServerHost> Server<H> {
    pub fn new(host: Arc<H>, options: ServerOptions) -> ChordResult<Self> {
        let (max_frame_length, handshake_timeout) = resolve_options(&options)?;
        let closing = Arc::new(AtomicBool::new(false));
        let inner = Arc::new_cyclic(|me: &Weak<ServerInner<H>>| {
            let publisher = me.clone();
            let reporter = me.clone();
            let sessions = SessionRouter::new(RouterOptions {
                host: host.clone(),
                server_id: options.server_id.clone(),
                closing: closing.clone(),
                publish: Arc::new(move |client, attachment| {
                    let server = publisher.clone();
                    Box::pin(async move {
                        if let Some(server) = server.upgrade() {
                            server.publish_attachment(client, attachment).await;
                        }
                    })
                }),
                report: Arc::new(move |error| {
                    if let Some(server) = reporter.upgrade() {
                        server.report_error(error);
                    }
                }),
            });
            ServerInner {
                host,
                listeners: options.listeners,
                server_id: options.server_id,
                max_frame_length,
                handshake_timeout,
                on_connection_count_changed: options.on_connection_count_changed,
                on_error: options.on_error,
                connections: Mutex::new(HashMap::new()),
                next_connection: AtomicU64::new(1),
                sessions,
                closing,
                lifecycle: Mutex::new(Lifecycle::default()),
                closed: watch::channel(None).0,
                me: me.clone(),
            }
        });
        Ok(Self { inner })
    }

    pub fn server_id(&self) -> &str {
        &self.inner.server_id
    }

    pub fn host(&self) -> &Arc<H> {
        &self.inner.host
    }

    /// Start every listener. A failure closes the listeners already started.
    pub async fn start(&self) -> ChordResult<()> {
        let task = {
            let mut lifecycle = lock(&self.inner.lifecycle);
            if lifecycle.started {
                return Err(ChordError::error("Server is already started"));
            }
            if lifecycle.starting.is_some() {
                return Err(ChordError::error("Server is already starting"));
            }
            if self.inner.closing.load(Ordering::SeqCst) {
                return Err(ChordError::error("Server is closing or closed"));
            }
            let inner = self.inner.clone();
            let task = spawn_shared(async move { inner.start_internal().await });
            lifecycle.starting = Some(task.clone());
            task
        };
        let result = task.await;
        lock(&self.inner.lifecycle).starting = None;
        result
    }

    /// Accept one established connection, for listeners and in-process transports.
    pub fn accept(&self, connection: Arc<dyn ByteConnection>) -> ByteConnectionHandler {
        self.inner.accept(connection)
    }

    /// Close listeners, connections, and every routed Session handle. Idempotent.
    pub async fn close(&self) -> ChordResult<()> {
        let task = {
            let mut lifecycle = lock(&self.inner.lifecycle);
            self.inner.closing.store(true, Ordering::SeqCst);
            let inner = self.inner.clone();
            lifecycle.close.get_or_insert_with(|| spawn_shared(async move { inner.close_internal().await })).clone()
        };
        task.await
    }

    /// Resolves after shutdown, or fails when listener or routed-Session cleanup fails.
    pub async fn closed(&self) -> ChordResult<()> {
        let mut receiver = self.inner.closed.subscribe();
        match receiver.wait_for(Option::is_some).await {
            Ok(value) => value.clone().unwrap_or(Ok(())),
            Err(_) => Ok(()),
        }
    }

    /// The number of open connections.
    pub fn connection_count(&self) -> usize {
        lock(&self.inner.connections).len()
    }
}

impl<H: ServerHost> ServerInner<H> {
    fn me(&self) -> Arc<Self> {
        self.me.upgrade().unwrap_or_else(|| unreachable!("server methods run on a live Arc"))
    }

    fn acceptor(&self) -> ByteConnectionAcceptor {
        let weak = self.me.clone();
        Arc::new(move |connection: Arc<dyn ByteConnection>| match weak.upgrade() {
            Some(inner) => inner.accept(connection),
            None => {
                tokio::spawn(connection.close(None));
                ByteConnectionHandler::new(|_| {}, || {}, |_| {})
            }
        })
    }

    async fn start_internal(&self) -> ChordResult<()> {
        // Like an async JavaScript start, never finish within the caller's first poll.
        tokio::task::yield_now().await;
        let acceptor = self.acceptor();
        let mut started = Vec::new();
        for listener in &self.listeners {
            if let Err(error) = listener.start(acceptor.clone()).await {
                self.closing.store(true, Ordering::SeqCst);
                let closes = started.iter().map(|listener: &Arc<dyn ServerListener>| listener.close());
                let mut cleanup: Vec<ChordError> =
                    futures::future::join_all(closes).await.into_iter().filter_map(Result::err).collect();
                if let Err(state_error) = self.close_server_state().await {
                    cleanup.push(state_error);
                }
                if cleanup.is_empty() {
                    self.settle_closed(Ok(()));
                    return Err(error);
                }
                let failure = ChordError::with_cleanup(error, cleanup, "Server startup and cleanup failed");
                self.settle_closed(Err(failure.clone()));
                return Err(failure);
            }
            started.push(listener.clone());
        }
        lock(&self.lifecycle).started = true;
        Ok(())
    }

    async fn close_internal(&self) -> ChordResult<()> {
        let starting = lock(&self.lifecycle).starting.clone();
        if let Some(starting) = starting {
            let _ = starting.await;
        }
        let closes = self.listeners.iter().map(|listener| listener.close());
        let mut errors: Vec<ChordError> =
            futures::future::join_all(closes).await.into_iter().filter_map(Result::err).collect();
        if let Err(error) = self.close_server_state().await {
            errors.push(error);
        }
        lock(&self.lifecycle).started = false;
        if errors.is_empty() {
            self.settle_closed(Ok(()));
            return Ok(());
        }
        let failure = if errors.len() == 1 {
            errors.remove(0)
        } else {
            ChordError::Aggregate { message: "Server shutdown failed".into(), errors }
        };
        self.settle_closed(Err(failure.clone()));
        Err(failure)
    }

    async fn close_server_state(&self) -> ChordResult<()> {
        let connections: Vec<Arc<Conn>> = lock(&self.connections).values().cloned().collect();
        for connection in &connections {
            connection.begin_closing();
        }
        futures::future::join_all(connections.iter().map(|connection| self.close_connection(connection, None))).await;
        for connection in &connections {
            self.disconnect(connection);
        }
        let result = self.sessions.close(Context::background()).await;
        lock(&self.connections).clear();
        result
    }

    fn settle_closed(&self, result: ChordResult<()>) {
        self.closed.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(result);
            true
        });
    }

    pub(crate) fn report_error(&self, error: &ChordError) {
        if let Some(on_error) = &self.on_error {
            on_error(error);
        }
    }

    fn notify_connection_count_changed(&self) {
        if let Some(callback) = &self.on_connection_count_changed {
            callback(lock(&self.connections).len());
        }
    }
}
