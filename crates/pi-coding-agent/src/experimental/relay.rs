//! The minimal relay interface the server needs. TODO(step 2): radius-relay.ts and radius-auth.ts provide
//! the Radius relay host and its auth resolver behind this interface.

use std::path::PathBuf;
use std::sync::Arc;

use pi_chord::{BoxFuture, ChordResult};

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
