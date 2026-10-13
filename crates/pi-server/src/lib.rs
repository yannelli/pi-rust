//! Experimental local server that routes clients to application-hosted durable pi Sessions.
//! A Rust port of upstream `@earendil-works/pi-server`.

mod connection;
mod errors;
mod listener;
mod server;
mod session_router;
mod task;
pub mod testing;
pub mod transports;
mod types;
mod uuid;

pub use connection::{ByteConnection, ByteConnectionAcceptor, ByteConnectionHandler};
pub use errors::{INTERNAL_SERVER_ERROR_MESSAGE, ServerError, ServerErrorCode, validation_error};
pub use listener::ServerListener;
pub use server::Server;
pub use types::{
    BasicSessionMetadata, ErrorReporter, RoutedServerPresentation, RoutedServerServiceAttachment,
    RoutedServerServiceHost, RoutedServiceAttachment, RoutedSessionAttachment, RoutedSessionHandle, ServerHost,
    ServerOptions, SessionMetadata,
};
pub use uuid::random_uuid;
