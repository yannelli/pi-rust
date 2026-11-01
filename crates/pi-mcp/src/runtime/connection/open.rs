//! Opening a connection: one try per transport, retries after transient HTTP errors, and the
//! listeners that keep tools current.

use std::path::Path;
use std::sync::{Arc, MutexGuard};
use std::time::Duration;

use super::{CONNECT_RETRY_DELAYS, Inner, McpServerConnection, ServerState, Status};
use crate::client::{McpClient, McpClientOptions, RequestOptions};
use crate::config::McpConfigScope;
use crate::error::{McpError, OAuthError};
use crate::transport::SharedTransport;
use crate::types::{ResourceTemplate, Root, Tool};

const STDERR_TAIL_CHARS: usize = 2_000;

/// Servers that do not implement `resources/templates/list` have no templates.
pub(super) async fn list_templates(
    client: &McpClient,
    options: RequestOptions,
) -> Result<Vec<ResourceTemplate>, McpError> {
    match client.list_resource_templates(options).await {
        Err(error) if error.is_method_not_found() => Ok(Vec::new()),
        result => result,
    }
}

fn stderr_tail(transport: &SharedTransport) -> Option<String> {
    let stderr = transport.stderr()?;
    let trimmed = stderr.trim();
    let skip = trimmed.chars().count().saturating_sub(STDERR_TAIL_CHARS);
    let tail: String = trimmed.chars().skip(skip).collect();
    (!tail.is_empty()).then_some(tail)
}

fn root(cwd: &Path) -> Root {
    let uri = url::Url::from_file_path(cwd).map_or_else(|()| format!("file://{}", cwd.display()), String::from);
    Root { uri, name: cwd.file_name().map(|name| name.to_string_lossy().into_owned()) }
}

struct Listed {
    tools: Vec<Tool>,
    has_resources: bool,
}

fn is_current(status: &Status, client: &McpClient) -> bool {
    status.client.as_ref().is_some_and(|current| current.same(client))
}

impl Inner {
    pub(super) fn lock(&self) -> MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn handle(self: &Arc<Self>) -> McpServerConnection {
        McpServerConnection { inner: self.clone() }
    }

    pub(super) fn changed(self: &Arc<Self>) {
        if let Some(on_change) = &self.options.on_change {
            on_change(&self.handle());
        }
    }

    fn tools_changed(self: &Arc<Self>) {
        if let Some(on_tools) = &self.options.on_tools {
            on_tools(&self.handle());
        }
    }

    /// Resolves once a connect in progress finished, whatever its result.
    pub(super) async fn settle_opening(&self) {
        let opening = self.opening.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        if let Some(opening) = opening {
            let _ = opening.await;
        }
    }

    pub(super) fn sign_in_required_message(&self) -> String {
        let entry = &self.options.entry;
        let command = match entry.config.auth_provider() {
            Some(provider) => format!("/login {provider}"),
            None => "/mcp".into(),
        };
        format!("MCP server \"{}\" requires sign-in. Run {command} to sign in.", entry.name)
    }

    /// Servers that still reject the request after a refresh need the user to sign in again.
    pub(super) fn needs_sign_in(&self, error: &McpError) -> bool {
        matches!(error, McpError::OAuth(OAuthError::AuthorizationRequired))
            || (self.auth.is_some() && matches!(error, McpError::AuthRequired { .. }))
    }

    pub(super) fn mark_needs_auth(self: &Arc<Self>) {
        {
            let mut status = self.lock();
            if self.shutdown.is_cancelled() {
                return;
            }
            status.state = ServerState::NeedsAuth;
            status.error = None;
        }
        self.changed();
    }

    pub(super) async fn drop_client(&self, client: &McpClient) {
        {
            let mut status = self.lock();
            if is_current(&status, client) {
                status.client = None;
            }
        }
        client.close().await;
    }

    pub(super) async fn open(self: Arc<Self>) -> Result<McpClient, McpError> {
        {
            let mut status = self.lock();
            // A `close` that ran before this task started did not wait for it.
            if self.shutdown.is_cancelled() {
                return Err(McpError::other(format!("MCP server \"{}\" is shut down", self.options.entry.name)));
            }
            status.state = ServerState::Connecting;
        }
        self.changed();
        let retries: &[Duration] = if self.options.entry.config.is_http() { &CONNECT_RETRY_DELAYS } else { &[] };
        let mut retry = 0;
        loop {
            self.lock().stderr_tail = None;
            let error = match self.connect_once().await {
                Ok(client) => return Ok(client),
                Err(error) => error,
            };
            let delay = retries.get(retry).filter(|_| !self.shutdown.is_cancelled() && error.is_transient());
            let Some(delay) = delay else { return Err(self.connect_failed(error)) };
            retry += 1;
            tokio::select! {
                () = tokio::time::sleep(*delay) => {}
                () = self.shutdown.cancelled() => return Err(self.connect_failed(error)),
            }
        }
    }

    async fn connect_once(self: &Arc<Self>) -> Result<McpClient, McpError> {
        let options = &self.options;
        let mut client_options = McpClientOptions::new("pi", options.client_version.clone());
        client_options.request_timeout = Some(self.handle().timeout());
        client_options.roots = Some(vec![root(&options.cwd)]);
        let client = McpClient::new(client_options);
        // Messages from servers an ACP client supplied are not written to the log file.
        let log = options.log.clone().filter(|_| options.entry.scope != Some(McpConfigScope::Client));
        if let Some(log) = log {
            let name = options.entry.name.clone();
            client.on_notification("notifications/message", move |params| log.write(&name, params));
        }
        let transport = match (options.create_transport)(&options.entry, &options.cwd, self.auth.clone()) {
            Ok(transport) => transport,
            Err(error) => {
                client.close().await;
                return Err(error);
            }
        };
        // Shutting down stops the connect: closing the client fails its pending requests.
        let result = tokio::select! {
            result = self.initialize(&client, &transport) => result,
            () = self.shutdown.cancelled() => Err(McpError::closed()),
        };
        let error = match result.and_then(|listed| self.publish(&client, listed)) {
            Ok(()) => {
                self.tools_changed();
                self.changed();
                return Ok(client);
            }
            Err(error) => error,
        };
        client.close().await;
        // Shutdown can win the select before `initialize` hands the transport to the client.
        transport.close().await;
        let tail = stderr_tail(&transport);
        self.lock().stderr_tail = tail;
        Err(error)
    }

    /// Install a connected client, unless `close` began: it cancels `shutdown` before it takes the
    /// status lock, so a client published under that lock is one `close` takes and closes.
    fn publish(&self, client: &McpClient, listed: Listed) -> Result<(), McpError> {
        let mut status = self.lock();
        if self.shutdown.is_cancelled() {
            return Err(McpError::other("shut down while connecting"));
        }
        if !client.is_connected() {
            return Err(McpError::other("connection closed during setup"));
        }
        status.client = Some(client.clone());
        status.tools = listed.tools;
        status.has_resources = listed.has_resources;
        status.instructions = client.instructions().map(|text| text.trim().to_string()).filter(|text| !text.is_empty());
        status.state = ServerState::Connected;
        status.error = None;
        Ok(())
    }

    async fn initialize(self: &Arc<Self>, client: &McpClient, transport: &SharedTransport) -> Result<Listed, McpError> {
        client.connect(transport.clone()).await?;
        let weak = Arc::downgrade(self);
        let (weak_tools, weak_client_tools) = (weak.clone(), client.downgrade());
        client.on_notification("notifications/tools/list_changed", move |_| {
            let (Some(inner), Some(client)) = (weak_tools.upgrade(), weak_client_tools.upgrade()) else { return };
            tokio::spawn(inner.refresh_tools(client));
        });
        let (weak_client, closed_transport) = (client.downgrade(), transport.clone());
        client.on_close(move || {
            if let (Some(inner), Some(client)) = (weak.upgrade(), weak_client.upgrade()) {
                inner.handle_client_close(&client, &closed_transport);
            }
        });
        // Servers without the tools capability (prompts or resources only) do not answer tools/list.
        // Resources are not listed here: listing every one can take long (a large drive, say), and the
        // resource tools list and read them on demand.
        let capabilities = client.server_capabilities().unwrap_or_default();
        let tools = if capabilities.tools() { client.list_tools(RequestOptions::default()).await? } else { Vec::new() };
        Ok(Listed { tools, has_resources: capabilities.resources() })
    }

    fn connect_failed(self: &Arc<Self>, error: McpError) -> McpError {
        let needs_sign_in = self.needs_sign_in(&error);
        let message = {
            let mut status = self.lock();
            let closed = self.shutdown.is_cancelled();
            if needs_sign_in && !closed {
                status.state = ServerState::NeedsAuth;
                status.error = None;
                None
            } else {
                status.state = if closed { ServerState::Closed } else { ServerState::Failed };
                let message = match &status.stderr_tail {
                    Some(tail) => format!("{error}\n{tail}"),
                    None => error.to_string(),
                };
                status.error = Some(message.clone());
                Some(message)
            }
        };
        self.changed();
        match message {
            None => McpError::Other(self.sign_in_required_message()),
            Some(message) => {
                McpError::Other(format!("MCP server \"{}\" failed to connect: {message}", self.options.entry.name))
            }
        }
    }

    /// The transport dropped. The next call reconnects; until then the status shows why.
    fn handle_client_close(self: &Arc<Self>, client: &McpClient, transport: &SharedTransport) {
        {
            let mut status = self.lock();
            if self.shutdown.is_cancelled() || !is_current(&status, client) {
                return;
            }
            status.client = None;
            status.state = ServerState::Disconnected;
            status.error = Some(match stderr_tail(transport) {
                Some(tail) => format!("Connection closed\n{tail}"),
                None => "Connection closed".into(),
            });
        }
        self.changed();
    }

    async fn refresh_tools(self: Arc<Self>, client: McpClient) {
        match client.list_tools(RequestOptions::default()).await {
            Ok(tools) => {
                {
                    let mut status = self.lock();
                    if self.shutdown.is_cancelled() || !is_current(&status, &client) {
                        return;
                    }
                    status.tools = tools;
                }
                self.tools_changed();
            }
            Err(error) => self.lock().error = Some(format!("Failed to refresh tools: {error}")),
        }
        self.changed();
    }
}

#[cfg(test)]
mod tests;
