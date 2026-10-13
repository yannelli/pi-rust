//! Model Context Protocol client for the pi agent harness: protocol, stdio and streamable HTTP
//! transports, OAuth, `mcp.json` configuration, server connections, and tool adapters.

pub mod auth;
pub mod client;
pub mod config;
pub mod config_value;
pub mod content;
pub mod error;
pub mod jsonrpc;
pub mod log;
pub mod manager;
pub mod naming;
pub mod oauth;
pub mod output;
pub mod resources;
pub mod runtime;
pub mod section;
pub mod tools;
pub mod transport;
pub mod types;

pub use auth::{AuthProvider, StaticToken, UnauthorizedContext};
pub use client::{ClientState, McpClient, McpClientOptions, RequestOptions};
pub use config::{
    LoadedMcpConfig, McpConfigScope, McpExposure, McpServerConfig, McpServerEntry, get_mcp_tool_exposure,
    load_mcp_config, mcp_namespace,
};
pub use content::{CallToolResult, ContentBlock, to_llm_content};
pub use error::{McpError, OAuthError};
pub use jsonrpc::{JsonRpcErrorObject, JsonRpcId, JsonRpcMessage};
pub use log::McpServerLog;
pub use manager::{McpAgentTool, McpManager, McpManagerOptions, McpServerStatus, NotifyLevel};
pub use naming::create_mcp_tool_name;
pub use output::{McpToolDetails, McpToolOutput, convert_mcp_result};
pub use resources::{McpResourceServer, McpResourceTool, create_mcp_resource_tools};
pub use section::{MCP_SERVERS_SECTION, McpServerListing, render_servers_section};
pub use tools::{McpTool, McpToolOptions, ToolAnnotations, ToolExposure, ToolNamespace, to_tool_exposure};
pub use transport::http::{ReconnectOptions, StreamableHttpOptions, StreamableHttpTransport};
pub use transport::memory::{InMemoryTransport, in_memory_pair};
pub use transport::process::kill_live_process_groups;
pub use transport::stdio::{StderrMode, StdioTransport, StdioTransportOptions};
pub use transport::{McpTransport, SharedTransport, TransportEvent, TransportSink};
pub use types::*;
