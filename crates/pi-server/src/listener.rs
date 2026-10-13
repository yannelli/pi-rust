//! Transports that supply connections to a server.

use async_trait::async_trait;
use pi_chord::ChordResult;

use crate::connection::ByteConnectionAcceptor;

/// Supplies established byte connections after any required transport authentication.
#[async_trait]
pub trait ServerListener: Send + Sync {
    /// Start listening and pass authorized connections to `accept`.
    async fn start(&self, accept: ByteConnectionAcceptor) -> ChordResult<()>;
    async fn close(&self) -> ChordResult<()>;
}
