//! OAuth for MCP HTTP servers: discovery, registration, PKCE, code exchange, refresh, and a
//! loopback callback server.

pub mod callback;
pub mod discovery;
pub mod flow;
pub mod http;
pub mod pkce;
pub mod provider;
pub mod requests;
pub mod types;

pub use callback::{
    CallbackPage, CallbackServer, CallbackServerFactory, CallbackServerOptions, LoopbackCallbackFactory,
    LoopbackCallbackServer, OAuthCallback, RenderPage,
};
pub use discovery::{
    authorization_server_discovery_urls, discover_authorization_server_metadata, discover_oauth_server_info,
    discover_protected_resource_metadata, parse_www_authenticate, resource_url_from_server_url, select_resource,
};
pub use flow::{
    ClientMetadataDocument, InvalidateKind, OAuthClientProvider, OAuthFlowOptions, OAuthFlowResult, authorize_mcp,
};
pub use http::OAuthHttp;
pub use provider::{
    McpOAuthProvider, McpOAuthProviderOptions, McpOAuthState, McpOAuthStateStore, MemoryOAuthStateStore,
    OAuthProviderAuth,
};
pub use requests::{
    AuthorizationRequest, TokenRequest, exchange_authorization_code, refresh_authorization, register_client,
    secure_endpoint, start_authorization, step_up_scope,
};
pub use types::*;
