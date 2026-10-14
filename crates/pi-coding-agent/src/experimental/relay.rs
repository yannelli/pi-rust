//! The relay interface the server needs. `radius_relay` provides the Radius relay host behind it.

use std::path::PathBuf;
use std::sync::Arc;

use pi_chord::{BoxFuture, ChordResult};
use pi_server::ByteConnectionAcceptor;

/// Relay connection states reported to the operator (upstream `RadiusRelayHostStatus`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RadiusRelayHostStatus {
    Connecting,
    Connected,
    NotAuthenticated,
    Retrying { error: String },
}

pub type RelayStatusListener = Arc<dyn Fn(&RadiusRelayHostStatus) + Send + Sync>;

pub struct RelayHostOptions {
    pub server_id: String,
    /// The server's public socket, which relayed connections reach.
    pub socket_path: PathBuf,
    /// The server's `accept`, for connections the relay carries.
    pub accept: ByteConnectionAcceptor,
    pub on_status: Option<RelayStatusListener>,
}

pub trait RelayHost: Send + Sync {
    fn start(&self);
    fn close(&self) -> BoxFuture<ChordResult<()>>;
}

pub type RelayHostFactory = Arc<dyn Fn(RelayHostOptions) -> Arc<dyn RelayHost> + Send + Sync>;

/// A local-only server: no relay.
pub struct NoRelayHost;

impl RelayHost for NoRelayHost {
    fn start(&self) {}

    fn close(&self) -> BoxFuture<ChordResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
