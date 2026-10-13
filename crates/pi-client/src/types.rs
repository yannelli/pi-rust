//! Client options and connection-state types.

use std::fmt;
use std::sync::Arc;

use pi_chord::ChordError;
use pi_protocol::SessionTarget;

use crate::errors::ClientError;
use crate::transport::ByteTransportFactory;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
}

impl ConnectionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
        }
    }
}

impl fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct ConnectionStateChange {
    pub state: ConnectionState,
    pub error: Option<ClientError>,
}

/// Receives connection state changes.
pub type ConnectionStateListener = Arc<dyn Fn(&ConnectionStateChange) + Send + Sync>;
/// Receives the live attachment route, or `None` when detached.
pub type AttachmentChangeListener = Arc<dyn Fn(Option<&SessionTarget>) + Send + Sync>;
/// Receives service-update listener failures without letting them affect client state.
pub type ListenerErrorHandler = Arc<dyn Fn(&ChordError) + Send + Sync>;

#[derive(Clone)]
pub struct ClientOptions {
    pub transport_factory: ByteTransportFactory,
    /// Logical server identity expected at the physical endpoint.
    pub server_id: String,
    pub max_frame_length: Option<u32>,
    pub on_listener_error: Option<ListenerErrorHandler>,
}

impl ClientOptions {
    pub fn new(server_id: impl Into<String>, transport_factory: ByteTransportFactory) -> Self {
        Self { transport_factory, server_id: server_id.into(), max_frame_length: None, on_listener_error: None }
    }
}
