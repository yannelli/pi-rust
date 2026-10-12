//! Model Context Protocol client for the pi agent harness: protocol, stdio and streamable HTTP
//! transports, OAuth, `mcp.json` configuration, server connections, and tool adapters.

pub mod auth;
pub mod client;
pub mod content;
pub mod error;
pub mod jsonrpc;
pub mod oauth;
pub mod transport;
pub mod types;

pub use auth::{AuthProvider, StaticToken, UnauthorizedContext};
pub use client::{ClientState, McpClient, McpClientOptions, RequestOptions};
pub use content::{CallToolResult, ContentBlock, to_llm_content};
pub use error::{McpError, OAuthError};
pub use jsonrpc::{JsonRpcErrorObject, JsonRpcId, JsonRpcMessage};
pub use transport::http::{ReconnectOptions, StreamableHttpOptions, StreamableHttpTransport};
pub use transport::memory::{InMemoryTransport, in_memory_pair};
pub use transport::process::kill_live_process_groups;
pub use transport::stdio::{StderrMode, StdioTransport, StdioTransportOptions};
pub use transport::{McpTransport, SharedTransport, TransportEvent, TransportSink};
pub use types::*;
