//! Ordered byte transports the client speaks the protocol over.

use std::sync::Arc;

use pi_chord::BoxFuture;

use crate::errors::ClientError;

/// One connected, authenticated byte stream.
pub trait ByteTransport: Send + Sync {
    /// Queue one chunk before returning; chunks are delivered in call order. The future
    /// reports delivery. Implementations may call the handlers from inside `send`.
    fn send(&self, chunk: Vec<u8>) -> BoxFuture<Result<(), ClientError>>;
    /// Close the transport. Repeated calls are harmless.
    fn close(&self);
}

type DataFn = Arc<dyn Fn(&[u8]) + Send + Sync>;
type CloseFn = Arc<dyn Fn() + Send + Sync>;
type ErrorFn = Arc<dyn Fn(ClientError) + Send + Sync>;

/// Inbound callbacks for one transport. Exactly one terminal callback is expected.
#[derive(Clone)]
pub struct ByteTransportHandlers {
    data: DataFn,
    close: CloseFn,
    error: ErrorFn,
}

impl ByteTransportHandlers {
    pub fn new(
        on_data: impl Fn(&[u8]) + Send + Sync + 'static,
        on_close: impl Fn() + Send + Sync + 'static,
        on_error: impl Fn(ClientError) + Send + Sync + 'static,
    ) -> Self {
        Self { data: Arc::new(on_data), close: Arc::new(on_close), error: Arc::new(on_error) }
    }

    /// Deliver an arbitrary inbound byte chunk.
    pub fn on_data(&self, chunk: &[u8]) {
        (self.data)(chunk)
    }

    /// Report an orderly terminal close.
    pub fn on_close(&self) {
        (self.close)()
    }

    /// Report a terminal transport failure.
    pub fn on_error(&self, error: ClientError) {
        (self.error)(error)
    }
}

/// Creates a fresh connected transport for each connection.
pub type ByteTransportFactory =
    Arc<dyn Fn(ByteTransportHandlers) -> BoxFuture<Result<Arc<dyn ByteTransport>, ClientError>> + Send + Sync>;
