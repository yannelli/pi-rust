//! The part of the MCP integration that talks to servers: connections, transports, OAuth sign-in,
//! and the credential store.

pub mod auth;
pub mod connection;
pub(crate) mod lock;
pub mod signin;
pub mod store;
pub mod transport;

pub use auth::{McpAuthProvider, ProviderTokenAuth, ProviderTokenFn};
pub use connection::{ConnectionCallback, ConnectionOptions, McpServerConnection, ServerState};
pub use signin::{McpOAuthSettings, McpSignInPrompt, SignInOptions, sign_in_mcp_server};
pub use store::{
    AuthStorageBackend, FileAuthStorageBackend, InMemoryAuthStorageBackend, McpOAuthCredentialStore,
    McpOAuthServerStore,
};
pub use transport::{TransportFactory, create_default_transport, default_transport_factory};
