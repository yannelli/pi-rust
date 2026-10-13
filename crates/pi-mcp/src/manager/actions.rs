//! Actions on one server: sign in and out, reconnect, enable or disable, and change exposure.
//! Each returns the message to show when it failed.

use std::path::PathBuf;
use std::sync::Arc;

use futures::FutureExt;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{Inner, McpManager, Ready};
use crate::config::{McpConfigScope, McpExposure, McpServerConfigPatch};
use crate::error::OAuthError;
use crate::runtime::{McpSignInPrompt, ServerState, SignInOptions, sign_in_mcp_server};

impl McpManager {
    /// Sign in with the browser flow, then reconnect. `cancel` or the session's end stops the sign-in.
    pub async fn sign_in(
        &self,
        name: &str,
        prompt: Arc<dyn McpSignInPrompt>,
        cancel: Option<CancellationToken>,
    ) -> Option<String> {
        let (connection, session) = {
            let state = self.inner.lock();
            (state.named(name).and_then(|server| server.connection.clone()), state.session.clone())
        };
        let not_oauth = || format!("MCP server \"{name}\" does not use OAuth.");
        let Some(connection) = connection else { return Some(not_oauth()) };
        let Some(url) = connection.oauth_url() else { return Some(not_oauth()) };
        let token = session.child_token();
        if let Some(cancel) = cancel {
            let linked = token.clone();
            tokio::spawn(async move {
                tokio::select! {
                    () = cancel.cancelled() => linked.cancel(),
                    () = linked.cancelled() => {}
                }
            });
        }
        let settings = match connection.oauth_settings() {
            Ok(settings) => settings,
            Err(error) => return Some(format!("Sign-in failed: {error}")),
        };
        let server_url = match Url::parse(&url) {
            Ok(url) => url,
            Err(error) => return Some(format!("Sign-in failed: {error}")),
        };
        let options = SignInOptions {
            server_url,
            store: self.inner.credentials.for_server(name, &url),
            settings,
            challenge: connection.challenge(),
            prompt,
            cancel: token,
            callback_factory: self.inner.options.callback_factory.clone(),
            client: pi_ai::http_client().clone(),
        };
        let signed_in = self.inner.track(sign_in_mcp_server(options)).await;
        match signed_in.unwrap_or_else(|error| Err(OAuthError::other(error.to_string()))) {
            Ok(()) => {}
            Err(OAuthError::Cancelled) => return Some("Sign-in cancelled.".into()),
            Err(error) => return Some(format!("Sign-in failed: {error}")),
        }
        // The challenge that asked for this sign-in (for example for more scope) is answered.
        connection.clear_challenge();
        connection.reconnect().await.err().map(|error| format!("Signed in, but {error}"))
    }

    /// Delete the stored credentials and disconnect. Returns whether credentials were stored.
    pub async fn sign_out(&self, name: &str) -> bool {
        let Some(connection) = self.connection(name) else { return false };
        let Some(url) = connection.oauth_url() else { return false };
        let removed = self.inner.credentials.remove(name, &url);
        connection.sign_out().await;
        removed
    }

    pub async fn reconnect(&self, name: &str) -> Option<String> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        {
            let mut state = self.inner.lock();
            let Some(server) = state.servers.iter_mut().find(|server| server.entry.name == name) else {
                return Some(format!("No MCP server named \"{name}\"."));
            };
            let Some(connection) = server.connection.clone() else {
                return Some(format!("MCP server \"{name}\" is disabled."));
            };
            let previous = server.ready.clone();
            let task = tokio::spawn(async move {
                if let Some(previous) = previous {
                    previous.await;
                }
                let _ = sender.send(connection.reconnect().await);
            });
            let ready: Ready = async move {
                let _ = task.await;
            }
            .boxed()
            .shared();
            server.ready = Some(ready);
        }
        match receiver.await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(_) => Some("Reconnect stopped".into()),
        }
    }

    /// Enable or disable a server, saved to its config. `in_project` saves a project override.
    pub async fn set_enabled(&self, name: &str, enabled: bool, in_project: bool) -> Option<String> {
        let patch = McpServerConfigPatch { enabled: Some(enabled), exposure: None };
        if let Some(failed) = self.inner.save_config(name, &patch, in_project) {
            return Some(failed);
        }
        let (id, closing) = {
            let mut state = self.inner.lock();
            let Some(server) = state.servers.iter_mut().find(|server| server.entry.name == name) else {
                return Some(format!("No MCP server named \"{name}\"."));
            };
            if !enabled {
                server.attempt = None;
                let (connection, ready) = (server.connection.take(), server.ready.clone());
                if let Some(connection) = connection {
                    // A reconnect may still be closing its old client; closing the connection alone
                    // does not wait for it.
                    let task = tokio::spawn(async move {
                        connection.close().await;
                        if let Some(ready) = ready {
                            ready.await;
                        }
                    });
                    let closing: Ready = async move {
                        let _ = task.await;
                    }
                    .boxed()
                    .shared();
                    server.closing = Some(closing);
                }
            }
            (server.id, server.closing.clone())
        };
        if enabled {
            self.inner.start_connection(id, closing).await;
            return None;
        }
        self.inner.hide_tools(name);
        self.inner.emit_change();
        if let Some(closing) = closing {
            closing.clone().await;
            let mut state = self.inner.lock();
            if let Some(server) = state
                .server_mut(id)
                .filter(|server| server.closing.as_ref().is_some_and(|current| current.ptr_eq(&closing)))
            {
                server.closing = None;
            }
        }
        None
    }

    /// Change a server's exposure, saved to its config. Tools register again with it.
    pub fn set_exposure(&self, name: &str, exposure: McpExposure) -> Option<String> {
        let patch = McpServerConfigPatch { enabled: None, exposure: Some(exposure) };
        if let Some(failed) = self.inner.save_config(name, &patch, false) {
            return Some(failed);
        }
        let connection = self.connection(name).filter(|connection| connection.state() == ServerState::Connected);
        if let Some(connection) = connection {
            self.inner.register_tools(&connection);
        }
        self.inner.sync_resource_tools();
        self.inner.emit_change();
        None
    }

    /// Reconnect servers waiting for a sign-in whose credentials were stored since, for example by
    /// `pi mcp login` in another process. Call it when a turn starts.
    pub async fn reconnect_signed_in(&self) {
        let signed_in: Vec<_> = {
            let mut state = self.inner.lock();
            let waiting = std::mem::take(&mut state.tokens_at_sign_in);
            let (changed, unchanged): (Vec<_>, Vec<_>) =
                waiting.into_iter().partition(|(connection, tokens)| self.inner.stored_tokens(connection) != *tokens);
            state.tokens_at_sign_in = unchanged;
            changed.into_iter().map(|(connection, _)| connection).collect()
        };
        futures::future::join_all(signed_in.iter().map(|connection| connection.reconnect())).await;
    }

    /// Set or clear the message shown with a server, like the result of a failed action.
    pub fn set_message(&self, name: &str, message: Option<String>) {
        if let Some(server) = self.inner.lock().servers.iter_mut().find(|server| server.entry.name == name) {
            server.message = message;
        }
        self.inner.emit_change();
    }
}

impl Inner {
    /// Save a config change. Registered servers change for this session only.
    pub(super) fn save_config(&self, name: &str, patch: &McpServerConfigPatch, in_project: bool) -> Option<String> {
        let mut state = self.lock();
        let project_config = state.project_config.clone();
        let Some(server) = state.servers.iter_mut().find(|server| server.entry.name == name) else {
            return Some(format!("No MCP server named \"{name}\"."));
        };
        let override_path = if in_project {
            project_config.map(|path: PathBuf| path.display().to_string())
        } else {
            server.entry.override_path.clone()
        };
        let mut entry = server.entry.clone();
        entry.override_path = override_path;
        if entry.scope != Some(McpConfigScope::Extension)
            && let Err(error) = self.update_config(&entry, patch)
        {
            return Some(format!(
                "Could not update {}: {error}",
                entry.override_path.as_deref().unwrap_or(&entry.source)
            ));
        }
        if let Some(enabled) = patch.enabled {
            entry.config.enabled = Some(enabled);
        }
        if let Some(exposure) = patch.exposure {
            entry.config.exposure = Some(exposure);
        }
        server.entry = entry;
        None
    }
}
