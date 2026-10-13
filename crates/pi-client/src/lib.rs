//! Transport-neutral client for the experimental pi service protocol, plus a Unix-socket
//! transport. A Rust port of upstream `@earendil-works/pi-client`.

mod client;
mod connection;
mod errors;
mod state;
mod subscription;
mod transport;
mod types;
#[cfg(unix)]
pub mod unix;

pub use client::{Client, ListenerGuard};
pub use errors::ClientError;
pub use subscription::{ServiceSubscription, TargetResolver, create_client_service_transport};
pub use transport::{ByteTransport, ByteTransportFactory, ByteTransportHandlers};
pub use types::{
    AttachmentChangeListener, ClientOptions, ConnectionState, ConnectionStateChange, ConnectionStateListener,
    ListenerErrorHandler,
};
