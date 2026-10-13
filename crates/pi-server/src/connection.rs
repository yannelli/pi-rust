//! Established, authorized byte connections and the handler the server attaches to each.

use std::sync::Arc;

use pi_chord::{BoxFuture, ChordError, ChordResult};

/// An established, authorized ordered byte connection.
///
/// `send` queues its chunk before returning and must not call back into the handler.
pub trait ByteConnection: Send + Sync {
    fn closed(&self) -> bool;
    fn send(&self, chunk: Vec<u8>) -> BoxFuture<ChordResult<()>>;
    /// Close after queued output, optionally writing one final chunk first.
    fn close(&self, final_chunk: Option<Vec<u8>>) -> BoxFuture<ChordResult<()>>;
}

type DataFn = Arc<dyn Fn(&[u8]) + Send + Sync>;
type CloseFn = Arc<dyn Fn() + Send + Sync>;
type ErrorFn = Arc<dyn Fn(ChordError) + Send + Sync>;

/// Receives one connection's inbound bytes and terminal events.
#[derive(Clone)]
pub struct ByteConnectionHandler {
    on_data: DataFn,
    on_close: CloseFn,
    on_error: ErrorFn,
}

impl ByteConnectionHandler {
    pub fn new(
        on_data: impl Fn(&[u8]) + Send + Sync + 'static,
        on_close: impl Fn() + Send + Sync + 'static,
        on_error: impl Fn(ChordError) + Send + Sync + 'static,
    ) -> Self {
        Self { on_data: Arc::new(on_data), on_close: Arc::new(on_close), on_error: Arc::new(on_error) }
    }

    pub fn on_data(&self, chunk: &[u8]) {
        (self.on_data)(chunk)
    }

    pub fn on_close(&self) {
        (self.on_close)()
    }

    pub fn on_error(&self, error: ChordError) {
        (self.on_error)(error)
    }
}

/// Accepts one established connection and returns its handler.
pub type ByteConnectionAcceptor = Arc<dyn Fn(Arc<dyn ByteConnection>) -> ByteConnectionHandler + Send + Sync>;
