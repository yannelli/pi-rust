//! One accepted Unix-domain socket as an ordered [`ByteConnection`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::{BoxFuture, ChordError, ChordResult};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot, watch};

use crate::connection::ByteConnection;
use crate::task::{SharedTask, lock, spawn_shared};

enum Op {
    Write(Vec<u8>, oneshot::Sender<ChordResult<()>>),
    End(Option<Vec<u8>>),
}

#[derive(Default)]
struct State {
    closed: bool,
    closing: bool,
    pending_bytes: usize,
    close_task: Option<SharedTask<ChordResult<()>>>,
}

/// Writes queued chunks in order, then half-closes after an optional final chunk.
pub struct UnixByteConnection {
    queue: mpsc::UnboundedSender<Op>,
    state: Arc<Mutex<State>>,
    closed: watch::Sender<bool>,
    destroyed: watch::Sender<bool>,
    graceful_close_timeout: Duration,
    max_pending_bytes: usize,
}

fn closed_error(message: &str) -> ChordError {
    ChordError::error(message)
}

impl UnixByteConnection {
    /// Wrap the write half of an accepted socket and start its writer task.
    pub fn new<W>(writer: W, graceful_close_timeout: Duration, max_pending_bytes: usize) -> Arc<Self>
    where
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (queue, receiver) = mpsc::unbounded_channel();
        let connection = Arc::new(Self {
            queue,
            state: Arc::new(Mutex::new(State::default())),
            closed: watch::channel(false).0,
            destroyed: watch::channel(false).0,
            graceful_close_timeout,
            max_pending_bytes,
        });
        tokio::spawn(write_loop(writer, receiver, connection.state.clone(), connection.destroyed.subscribe()));
        connection
    }

    /// Record that the socket closed. Resolves any pending `close`.
    pub fn mark_closed(&self) {
        {
            let mut state = lock(&self.state);
            state.closed = true;
            state.closing = true;
        }
        self.closed.send_replace(true);
    }

    /// Tear the socket down immediately.
    pub fn destroy(&self) {
        self.destroyed.send_replace(true);
    }

    /// Resolves once the connection is destroyed.
    pub async fn destroyed(&self) {
        let mut receiver = self.destroyed.subscribe();
        let _ = receiver.wait_for(|destroyed| *destroyed).await;
    }

    pub fn is_destroyed(&self) -> bool {
        *self.destroyed.borrow()
    }
}

async fn write_loop<W>(
    mut writer: W,
    mut receiver: mpsc::UnboundedReceiver<Op>,
    state: Arc<Mutex<State>>,
    mut destroyed: watch::Receiver<bool>,
) where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    loop {
        let op = tokio::select! {
            op = receiver.recv() => op,
            _ = destroyed.wait_for(|destroyed| *destroyed) => None,
        };
        match op {
            None => break,
            Some(Op::Write(bytes, reply)) => {
                let result = tokio::select! {
                    result = writer.write_all(&bytes) => result.map_err(ChordError::other),
                    _ = destroyed.wait_for(|destroyed| *destroyed) => Err(closed_error("Unix connection closed during write")),
                };
                lock(&state).pending_bytes -= bytes.len();
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            Some(Op::End(final_chunk)) => {
                if let Some(bytes) = final_chunk {
                    let _ = writer.write_all(&bytes).await;
                }
                let _ = writer.shutdown().await;
            }
        }
    }
    receiver.close();
    while let Ok(op) = receiver.try_recv() {
        if let Op::Write(bytes, reply) = op {
            lock(&state).pending_bytes -= bytes.len();
            let _ = reply.send(Err(closed_error("Unix connection closed during write")));
        }
    }
}

impl ByteConnection for UnixByteConnection {
    fn closed(&self) -> bool {
        lock(&self.state).closed
    }

    fn send(&self, chunk: Vec<u8>) -> BoxFuture<ChordResult<()>> {
        let mut state = lock(&self.state);
        if state.closed || state.closing {
            return Box::pin(std::future::ready(Err(closed_error("Unix connection is closed"))));
        }
        if state.pending_bytes + chunk.len() > self.max_pending_bytes {
            return Box::pin(std::future::ready(Err(closed_error("Unix connection exceeded its pending byte limit"))));
        }
        state.pending_bytes += chunk.len();
        let length = chunk.len();
        let (reply, result) = oneshot::channel();
        if self.queue.send(Op::Write(chunk, reply)).is_err() {
            state.pending_bytes -= length;
            return Box::pin(std::future::ready(Err(closed_error("Unix connection is closed"))));
        }
        Box::pin(
            async move { result.await.unwrap_or_else(|_| Err(closed_error("Unix connection closed during write"))) },
        )
    }

    fn close(&self, final_chunk: Option<Vec<u8>>) -> BoxFuture<ChordResult<()>> {
        let mut state = lock(&self.state);
        if state.closed || self.is_destroyed() {
            drop(state);
            self.mark_closed();
            return Box::pin(std::future::ready(Ok(())));
        }
        if let Some(task) = &state.close_task {
            return Box::pin(task.clone());
        }
        state.closing = true;
        if self.queue.send(Op::End(final_chunk)).is_err() {
            self.destroy();
        }
        let mut closed = self.closed.subscribe();
        let (timeout, destroyed, marker) = (self.graceful_close_timeout, self.destroyed.clone(), self.closed.clone());
        let flag = self.state.clone();
        let task = spawn_shared(async move {
            if tokio::time::timeout(timeout, closed.wait_for(|closed| *closed)).await.is_err() {
                destroyed.send_replace(true);
                {
                    let mut state = lock(&flag);
                    state.closed = true;
                }
                marker.send_replace(true);
            }
            Ok(())
        });
        state.close_task = Some(task.clone());
        Box::pin(task)
    }
}
