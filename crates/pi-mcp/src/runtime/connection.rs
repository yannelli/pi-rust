//! One configured server. Connects in the background, reconnects lazily when a call finds the
//! connection gone, and asks for a sign-in when OAuth credentials stop working.

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::auth::{McpAuthProvider, ProviderTokenAuth, ProviderTokenFn};
use super::signin::McpOAuthSettings;
use super::store::McpOAuthCredentialStore;
use super::transport::{TransportFactory, default_transport_factory};
use crate::auth::AuthProvider;
use crate::client::{McpClient, RequestOptions};
use crate::config::McpServerEntry;
use crate::config_value::resolve_config_value_or_err;
use crate::content::CallToolResult;
use crate::error::McpError;
use crate::log::McpServerLog;
use crate::oauth::OAuthChallenge;
use crate::types::{
    ListResourceTemplatesResult, ListResourcesResult, ReadResourceResult, Resource, ResourceTemplate, Tool,
};

mod open;

const DEFAULT_TIMEOUT_SECONDS: f64 = 60.0;
/// Delays between tries to connect to an HTTP server that failed with a transient error.
const CONNECT_RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(250), Duration::from_millis(1000)];

/// `Disconnected`: the connection dropped (for example the stdio server exited); the next call reconnects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerState {
    Connecting,
    Connected,
    Disconnected,
    NeedsAuth,
    Failed,
    Closed,
}

impl fmt::Display for ServerState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::NeedsAuth => "needs-auth",
            Self::Failed => "failed",
            Self::Closed => "closed",
        })
    }
}

pub type ConnectionCallback = Arc<dyn Fn(&McpServerConnection) + Send + Sync>;

#[derive(Clone)]
pub struct ConnectionOptions {
    pub entry: McpServerEntry,
    /// Working directory of the session, for stdio servers and `roots/list`.
    pub cwd: PathBuf,
    pub create_transport: TransportFactory,
    pub credentials: McpOAuthCredentialStore,
    /// The current token of a pi provider, for servers with `auth.provider`.
    pub provider_token: Option<ProviderTokenFn>,
    /// Called when the server's tools or resources were (re)listed.
    pub on_tools: Option<ConnectionCallback>,
    /// Called when the state, error, or tools change.
    pub on_change: Option<ConnectionCallback>,
    /// Receives the server's `notifications/message`.
    pub log: Option<Arc<McpServerLog>>,
    pub client_version: String,
    pub http_client: Option<reqwest::Client>,
}

impl ConnectionOptions {
    pub fn new(entry: McpServerEntry, cwd: PathBuf, credentials: McpOAuthCredentialStore) -> Self {
        Self {
            entry,
            cwd,
            create_transport: default_transport_factory(),
            credentials,
            provider_token: None,
            on_tools: None,
            on_change: None,
            log: None,
            client_version: env!("CARGO_PKG_VERSION").into(),
            http_client: None,
        }
    }
}

struct Status {
    state: ServerState,
    error: Option<String>,
    tools: Vec<Tool>,
    has_resources: bool,
    resources: Vec<Resource>,
    resource_templates: Vec<ResourceTemplate>,
    instructions: Option<String>,
    challenge: Option<OAuthChallenge>,
    client: Option<McpClient>,
    stderr_tail: Option<String>,
}

type Opening = Shared<BoxFuture<'static, Result<McpClient, McpError>>>;

struct Inner {
    options: ConnectionOptions,
    oauth: Option<Arc<McpAuthProvider>>,
    auth: Option<Arc<dyn AuthProvider>>,
    shutdown: CancellationToken,
    status: Mutex<Status>,
    opening: Mutex<Option<Opening>>,
}

#[derive(Clone)]
pub struct McpServerConnection {
    inner: Arc<Inner>,
}

fn oauth_settings(entry: &McpServerEntry) -> Result<McpOAuthSettings, String> {
    let Some(oauth) = entry.config.oauth.as_ref().filter(|_| entry.config.is_http()) else {
        return Ok(McpOAuthSettings::default());
    };
    let client_secret = match &oauth.client_secret {
        Some(secret) => Some(resolve_config_value_or_err(
            secret,
            &format!("MCP server \"{}\" oauth.clientSecret", entry.name),
            None,
        )?),
        None => None,
    };
    let metadata_url = oauth.auth_server_metadata_url.as_deref().map(Url::parse).transpose();
    Ok(McpOAuthSettings {
        client_id: oauth.client_id.clone(),
        client_secret,
        callback_port: oauth.callback_port,
        callback_url: oauth.callback_url.clone(),
        scope: oauth.scope.clone(),
        client_name: oauth.client_name.clone(),
        client_registration: oauth.client_registration.clone(),
        auth_server_metadata_url: metadata_url.map_err(|error| error.to_string())?,
    })
}

impl McpServerConnection {
    pub fn new(options: ConnectionOptions) -> Self {
        let inner = Arc::new_cyclic(|weak: &Weak<Inner>| {
            let entry = options.entry.clone();
            let oauth_url =
                entry.config.url.clone().filter(|_| entry.config.uses_oauth()).and_then(|url| Url::parse(&url).ok());
            let oauth = oauth_url.map(|url| {
                let store = options.credentials.for_server(&entry.name, url.as_str());
                let settings_entry = entry.clone();
                let weak = weak.clone();
                Arc::new(McpAuthProvider::new(
                    url,
                    store,
                    Arc::new(move || oauth_settings(&settings_entry)),
                    Arc::new(move |challenge| {
                        if let Some(inner) = weak.upgrade() {
                            inner.lock().challenge = Some(challenge);
                        }
                    }),
                    options.http_client.clone().unwrap_or_else(|| pi_ai::http_client().clone()),
                ))
            });
            let auth: Option<Arc<dyn AuthProvider>> = match (&oauth, entry.config.auth_provider()) {
                (Some(oauth), _) => Some(oauth.clone()),
                (None, Some(provider)) => Some(Arc::new(ProviderTokenAuth {
                    provider: provider.into(),
                    token: options.provider_token.clone(),
                })),
                (None, None) => None,
            };
            let status = Status {
                state: ServerState::Connecting,
                error: None,
                tools: Vec::new(),
                has_resources: false,
                resources: Vec::new(),
                resource_templates: Vec::new(),
                instructions: None,
                challenge: None,
                client: None,
                stderr_tail: None,
            };
            Inner {
                options,
                oauth,
                auth,
                shutdown: CancellationToken::new(),
                status: Mutex::new(status),
                opening: Mutex::new(None),
            }
        });
        Self { inner }
    }

    pub fn name(&self) -> &str {
        &self.inner.options.entry.name
    }

    pub fn entry(&self) -> &McpServerEntry {
        &self.inner.options.entry
    }

    pub fn same(&self, other: &McpServerConnection) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn state(&self) -> ServerState {
        self.inner.lock().state
    }

    pub fn error(&self) -> Option<String> {
        self.inner.lock().error.clone()
    }

    pub fn tools(&self) -> Vec<Tool> {
        self.inner.lock().tools.clone()
    }

    /// Whether the server offers resources.
    pub fn has_resources(&self) -> bool {
        self.inner.lock().has_resources
    }

    /// Resources listed at the last connect or change, without MCP App resources.
    pub fn resources(&self) -> Vec<Resource> {
        self.inner.lock().resources.clone()
    }

    pub fn resource_templates(&self) -> Vec<ResourceTemplate> {
        self.inner.lock().resource_templates.clone()
    }

    /// Server instructions from `initialize`, describing its tools as a group.
    pub fn instructions(&self) -> Option<String> {
        self.inner.lock().instructions.clone()
    }

    /// Last OAuth challenge from the server; sign-in uses its resource metadata URL and scope.
    pub fn challenge(&self) -> Option<OAuthChallenge> {
        self.inner.lock().challenge.clone()
    }

    pub fn clear_challenge(&self) {
        self.inner.lock().challenge = None;
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs_f64(self.inner.options.entry.config.timeout.unwrap_or(DEFAULT_TIMEOUT_SECONDS))
    }

    /// Server URL when the server authenticates with OAuth.
    pub fn oauth_url(&self) -> Option<String> {
        let config = &self.inner.options.entry.config;
        config.url.clone().filter(|_| config.uses_oauth())
    }

    /// OAuth settings with the client secret resolved.
    pub fn oauth_settings(&self) -> Result<McpOAuthSettings, String> {
        oauth_settings(&self.inner.options.entry)
    }

    pub fn credentials(&self) -> &McpOAuthCredentialStore {
        &self.inner.options.credentials
    }

    pub async fn get_client(&self) -> Result<McpClient, McpError> {
        if self.inner.shutdown.is_cancelled() {
            return Err(McpError::other(format!("MCP server \"{}\" is shut down", self.name())));
        }
        if let Some(client) = self.inner.lock().client.clone().filter(McpClient::is_connected) {
            return Ok(client);
        }
        let opening = {
            let mut opening = self.inner.opening.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            opening
                .get_or_insert_with(|| {
                    let task = tokio::spawn(self.inner.clone().open());
                    async move { task.await.unwrap_or_else(|error| Err(McpError::other(error.to_string()))) }
                        .boxed()
                        .shared()
                })
                .clone()
        };
        let result = opening.clone().await;
        let mut current = self.inner.opening.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if current.as_ref().is_some_and(|current| current.ptr_eq(&opening)) {
            *current = None;
        }
        result
    }

    fn request_options(&self, options: RequestOptions) -> RequestOptions {
        RequestOptions { timeout: options.timeout.or(Some(self.timeout())), ..options }
    }

    pub async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
        options: RequestOptions,
    ) -> Result<CallToolResult, McpError> {
        let options = self.request_options(options);
        self.with_client(false, |client| {
            let options = options.clone();
            let args = args.clone();
            async move { client.call_tool(name, Some(args), options).await }
        })
        .await
    }

    pub async fn read_resource(&self, uri: &str, options: RequestOptions) -> Result<ReadResourceResult, McpError> {
        let options = self.request_options(options);
        self.with_client(true, |client| {
            let options = options.clone();
            async move { client.read_resource(uri, options).await }
        })
        .await
    }

    pub async fn resources_page(
        &self,
        cursor: Option<&str>,
        options: RequestOptions,
    ) -> Result<ListResourcesResult, McpError> {
        let options = self.request_options(options);
        self.with_client(true, |client| {
            let options = options.clone();
            async move { client.list_resources_page(cursor, options).await }
        })
        .await
    }

    /// Servers that do not implement `resources/templates/list` have no templates.
    pub async fn resource_templates_page(
        &self,
        cursor: Option<&str>,
        options: RequestOptions,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let options = self.request_options(options);
        self.with_client(true, |client| {
            let options = options.clone();
            async move {
                match client.list_resource_templates_page(cursor, options).await {
                    Err(error) if error.is_method_not_found() => Ok(ListResourceTemplatesResult::default()),
                    result => result,
                }
            }
        })
        .await
    }

    pub async fn all_resources(&self, options: RequestOptions) -> Result<Vec<Resource>, McpError> {
        let options = self.request_options(options);
        self.with_client(true, |client| {
            let options = options.clone();
            async move { client.list_resources(options).await }
        })
        .await
    }

    pub async fn all_resource_templates(&self, options: RequestOptions) -> Result<Vec<ResourceTemplate>, McpError> {
        let options = self.request_options(options);
        self.with_client(true, |client| {
            let options = options.clone();
            async move { open::list_templates(&client, options).await }
        })
        .await
    }

    /// Run a request, reconnecting when needed. Read-only requests are retried once after a
    /// transient HTTP error; tool calls are not, since they may have run.
    async fn with_client<T, F, Fut>(&self, read_only: bool, run: F) -> Result<T, McpError>
    where
        F: Fn(McpClient) -> Fut,
        Fut: Future<Output = Result<T, McpError>>,
    {
        let mut attempt = 1;
        loop {
            let client = self.get_client().await?;
            let error = match run(client.clone()).await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            if read_only && attempt == 1 && matches!(error, McpError::Http { .. }) && error.is_transient() {
                tokio::time::sleep(CONNECT_RETRY_DELAYS[0]).await;
                attempt += 1;
                continue;
            }
            if matches!(error, McpError::SessionExpired { .. }) && attempt == 1 {
                // The server forgot the session, so it did not run the request. Retry on a new session;
                // the old client is detached but not closed, which would fail its other calls.
                let mut status = self.inner.lock();
                if status.client.as_ref().is_some_and(|current| current.same(&client)) {
                    status.client = None;
                }
                attempt += 1;
                continue;
            }
            if !self.inner.needs_sign_in(&error) {
                return Err(error);
            }
            self.inner.drop_client(&client).await;
            self.inner.mark_needs_auth();
            return Err(McpError::Other(self.inner.sign_in_required_message()));
        }
    }

    /// Connect again with fresh credentials, for example after signing in.
    pub async fn reconnect(&self) -> Result<(), McpError> {
        self.inner.settle_opening().await;
        let client = self.inner.lock().client.clone();
        if let Some(client) = client {
            self.inner.drop_client(&client).await;
        }
        self.get_client().await.map(|_| ())
    }

    /// Disconnect after the stored credentials were removed.
    pub async fn sign_out(&self) {
        self.inner.settle_opening().await;
        let client = self.inner.lock().client.clone();
        if let Some(client) = client {
            self.inner.drop_client(&client).await;
        }
        if !self.inner.shutdown.is_cancelled() {
            self.inner.mark_needs_auth();
        }
    }

    /// Resolves once no transport of this server is open, including one that was still connecting.
    pub async fn close(&self) {
        self.inner.shutdown.cancel();
        self.inner.lock().state = ServerState::Closed;
        self.inner.changed();
        let client = self.inner.lock().client.take();
        if let Some(client) = client {
            client.close().await;
        }
        self.inner.settle_opening().await;
        // A refresh the server already answered may have rotated the refresh token; exiting before
        // the new tokens are saved would lose the grant.
        if let Some(oauth) = &self.inner.oauth {
            oauth.settled().await;
        }
    }
}
