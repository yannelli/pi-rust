//! A [`ByteTransport`] over one Unix-domain socket.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::BoxFuture;
use pi_protocol::DEFAULT_MAX_FRAME_LENGTH;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch};

use crate::errors::ClientError;
use crate::state::lock;
use crate::transport::{ByteTransport, ByteTransportFactory, ByteTransportHandlers};

#[derive(Clone, Debug)]
pub struct UnixTransportOptions {
    pub path: PathBuf,
    /// Maximum queued output bytes. Defaults to four maximum frames.
    pub max_pending_bytes: Option<usize>,
}

impl UnixTransportOptions {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), max_pending_bytes: None }
    }
}

pub(crate) fn validate(options: &UnixTransportOptions) -> Result<usize, ClientError> {
    if options.path.as_os_str().is_empty() {
        return Err(ClientError::Type("Unix transport path must not be empty".into()));
    }
    let max_pending_bytes = options.max_pending_bytes.unwrap_or(DEFAULT_MAX_FRAME_LENGTH as usize * 4);
    if max_pending_bytes == 0 {
        return Err(ClientError::Type("Unix transport maxPendingBytes must be a positive safe integer".into()));
    }
    Ok(max_pending_bytes)
}

/// Create fresh Unix-domain socket transports for client connections.
pub fn create_unix_transport_factory(options: UnixTransportOptions) -> Result<ByteTransportFactory, ClientError> {
    let max_pending_bytes = validate(&options)?;
    let path = Arc::new(options.path);
    Ok(Arc::new(move |handlers| {
        let path = path.clone();
        Box::pin(async move { connect_unix_socket(&path, max_pending_bytes, handlers).await })
    }))
}

type Write = (Vec<u8>, oneshot::Sender<Result<(), ClientError>>);

struct UnixByteTransport {
    queue: mpsc::UnboundedSender<Write>,
    pending_bytes: Arc<AtomicUsize>,
    max_pending_bytes: usize,
    closed: AtomicBool,
    terminal: Arc<AtomicBool>,
    shutdown: watch::Sender<bool>,
    reader: Mutex<Option<tokio::task::AbortHandle>>,
}

fn closed_error(message: &str) -> ClientError {
    ClientError::Other(message.into())
}

pub(crate) async fn connect_unix_socket(
    path: &Path,
    max_pending_bytes: usize,
    handlers: ByteTransportHandlers,
) -> Result<Arc<dyn ByteTransport>, ClientError> {
    let stream = UnixStream::connect(path).await?;
    let (mut reader, mut writer) = stream.into_split();
    let (queue, mut writes) = mpsc::unbounded_channel::<Write>();
    let (shutdown, mut stopped) = watch::channel(false);
    let pending_bytes = Arc::new(AtomicUsize::new(0));
    let terminal = Arc::new(AtomicBool::new(false));

    let pending = pending_bytes.clone();
    let mut write_stopped = stopped.clone();
    tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                next = writes.recv() => next,
                _ = write_stopped.wait_for(|stopped| *stopped) => None,
            };
            let Some((bytes, reply)) = next else { break };
            let result = writer.write_all(&bytes).await.map_err(ClientError::from);
            pending.fetch_sub(bytes.len(), Ordering::SeqCst);
            let failed = result.is_err();
            let _ = reply.send(result);
            if failed {
                break;
            }
        }
        writes.close();
        while let Ok((bytes, reply)) = writes.try_recv() {
            pending.fetch_sub(bytes.len(), Ordering::SeqCst);
            let _ = reply.send(Err(closed_error("Unix transport closed during write")));
        }
    });

    let finished = terminal.clone();
    let read_task = tokio::spawn(async move {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = tokio::select! {
                read = reader.read(&mut buffer) => read,
                _ = stopped.wait_for(|stopped| *stopped) => return,
            };
            match read {
                Ok(count) if count > 0 => {
                    if !finished.load(Ordering::SeqCst) {
                        handlers.on_data(&buffer[..count]);
                    }
                }
                Ok(_) => {
                    if !finished.swap(true, Ordering::SeqCst) {
                        handlers.on_close();
                    }
                    return;
                }
                Err(error) => {
                    if !finished.swap(true, Ordering::SeqCst) {
                        handlers.on_error(error.into());
                    }
                    return;
                }
            }
        }
    });
    Ok(Arc::new(UnixByteTransport {
        queue,
        pending_bytes,
        max_pending_bytes,
        closed: AtomicBool::new(false),
        terminal,
        shutdown,
        reader: Mutex::new(Some(read_task.abort_handle())),
    }))
}

impl ByteTransport for UnixByteTransport {
    fn send(&self, chunk: Vec<u8>) -> BoxFuture<Result<(), ClientError>> {
        if self.closed.load(Ordering::SeqCst) {
            return Box::pin(std::future::ready(Err(closed_error("Unix transport is closed"))));
        }
        let length = chunk.len();
        if self.pending_bytes.load(Ordering::SeqCst) + length > self.max_pending_bytes {
            let error = closed_error("Unix transport exceeded its pending byte limit");
            return Box::pin(std::future::ready(Err(error)));
        }
        self.pending_bytes.fetch_add(length, Ordering::SeqCst);
        let (reply, result) = oneshot::channel();
        if self.queue.send((chunk, reply)).is_err() {
            self.pending_bytes.fetch_sub(length, Ordering::SeqCst);
            return Box::pin(std::future::ready(Err(closed_error("Unix transport is closed"))));
        }
        Box::pin(
            async move { result.await.unwrap_or_else(|_| Err(closed_error("Unix transport closed during write"))) },
        )
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.terminal.store(true, Ordering::SeqCst);
        self.shutdown.send_replace(true);
        if let Some(reader) = lock(&self.reader).take() {
            reader.abort();
        }
    }
}
