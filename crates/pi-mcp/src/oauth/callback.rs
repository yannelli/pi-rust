//! Loopback HTTP server that receives OAuth authorization responses. Behind a trait, so the app
//! can supply its own callback server.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::OAuthError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthCallback {
    pub code: String,
    pub state: String,
    pub iss: Option<String>,
}

/// Outcome shown on the browser page after the redirect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallbackPage {
    Ok,
    Failed { message: String, details: Option<String> },
}

pub type RenderPage = Arc<dyn Fn(&CallbackPage) -> String + Send + Sync>;

#[derive(Clone)]
pub struct CallbackServerOptions {
    /// Address to listen on. Default: `127.0.0.1`.
    pub host: String,
    /// Host name in the redirect URL, for example `localhost` while listening on `127.0.0.1`.
    pub redirect_host: Option<String>,
    /// Zero picks a free port.
    pub port: u16,
    pub path: String,
    /// More paths that receive the callback, like a server-specific path of a redirect URI.
    pub extra_paths: Vec<String>,
    pub timeout: Duration,
    /// Render the browser page as HTML. Default: plain text.
    pub render_page: Option<RenderPage>,
}

impl Default for CallbackServerOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            redirect_host: None,
            port: 0,
            path: "/callback".into(),
            extra_paths: Vec::new(),
            timeout: Duration::from_secs(5 * 60),
            render_page: None,
        }
    }
}

pub type CallbackFuture = BoxFuture<'static, Result<OAuthCallback, OAuthError>>;

#[async_trait]
pub trait CallbackServer: Send + Sync {
    fn redirect_url(&self) -> String;
    /// Register for the response with `state` and return a future that resolves with it. With
    /// `path`, a response on another path fails (RFC 9700 section 4.4.2.2).
    fn wait_for_callback(&self, state: &str, path: Option<&str>) -> Result<CallbackFuture, OAuthError>;
    async fn close(&self);
}

#[async_trait]
pub trait CallbackServerFactory: Send + Sync {
    async fn listen(&self, options: CallbackServerOptions) -> Result<Arc<dyn CallbackServer>, OAuthError>;
}

/// The built-in loopback callback server.
pub struct LoopbackCallbackFactory;

#[async_trait]
impl CallbackServerFactory for LoopbackCallbackFactory {
    async fn listen(&self, options: CallbackServerOptions) -> Result<Arc<dyn CallbackServer>, OAuthError> {
        Ok(Arc::new(LoopbackCallbackServer::listen(options).await?))
    }
}

struct Waiter {
    sender: oneshot::Sender<Result<OAuthCallback, OAuthError>>,
    path: Option<String>,
}

type Waiters = Arc<Mutex<HashMap<String, Waiter>>>;

pub struct LoopbackCallbackServer {
    redirect_url: String,
    timeout: Duration,
    waiters: Waiters,
    shutdown: CancellationToken,
}

fn lock(waiters: &Waiters) -> MutexGuard<'_, HashMap<String, Waiter>> {
    waiters.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl LoopbackCallbackServer {
    pub async fn listen(options: CallbackServerOptions) -> Result<Self, OAuthError> {
        let host = options.host.trim_start_matches('[').trim_end_matches(']').to_string();
        let listener = TcpListener::bind((host.as_str(), options.port))
            .await
            .map_err(|error| OAuthError::other(format!("Could not listen on {host}:{}: {error}", options.port)))?;
        let port = listener.local_addr().map_err(|error| OAuthError::other(error.to_string()))?.port();
        let redirect_host = options.redirect_host.clone().unwrap_or(host);
        let shown_host = if redirect_host.contains(':') { format!("[{redirect_host}]") } else { redirect_host };
        let redirect_url = format!("http://{shown_host}:{port}{}", options.path);
        let mut paths = vec![options.path.clone()];
        paths.extend(options.extra_paths.iter().cloned());
        let waiters: Waiters = Arc::default();
        let shutdown = CancellationToken::new();
        let handler = Arc::new(Handler {
            redirect_url: redirect_url.clone(),
            paths,
            waiters: waiters.clone(),
            render: options.render_page,
        });
        let stop = shutdown.clone();
        tokio::spawn(async move {
            loop {
                let socket = tokio::select! {
                    accepted = listener.accept() => match accepted {
                        Ok((socket, _)) => socket,
                        Err(_) => continue,
                    },
                    _ = stop.cancelled() => return,
                };
                tokio::spawn(handler.clone().serve(socket));
            }
        });
        Ok(Self { redirect_url, timeout: options.timeout, waiters, shutdown })
    }
}

#[async_trait]
impl CallbackServer for LoopbackCallbackServer {
    fn redirect_url(&self) -> String {
        self.redirect_url.clone()
    }

    fn wait_for_callback(&self, state: &str, path: Option<&str>) -> Result<CallbackFuture, OAuthError> {
        let (sender, receiver) = oneshot::channel();
        {
            let mut waiters = lock(&self.waiters);
            if waiters.contains_key(state) {
                return Err(OAuthError::other("OAuth state is already pending"));
            }
            waiters.insert(state.to_string(), Waiter { sender, path: path.map(str::to_string) });
        }
        let waiters = self.waiters.clone();
        let timeout = self.timeout;
        let state = state.to_string();
        Ok(Box::pin(async move {
            match tokio::time::timeout(timeout, receiver).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(OAuthError::other("OAuth callback server closed")),
                Err(_) => {
                    lock(&waiters).remove(&state);
                    Err(OAuthError::other("OAuth callback timed out"))
                }
            }
        }))
    }

    async fn close(&self) {
        self.shutdown.cancel();
        for (_, waiter) in lock(&self.waiters).drain() {
            let _ = waiter.sender.send(Err(OAuthError::other("OAuth callback server closed")));
        }
    }
}

impl Drop for LoopbackCallbackServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

struct Handler {
    redirect_url: String,
    paths: Vec<String>,
    waiters: Waiters,
    render: Option<RenderPage>,
}

fn plain_text(page: &CallbackPage) -> String {
    match page {
        CallbackPage::Ok => "Authorization complete. You may close this window.".into(),
        CallbackPage::Failed { message, details: Some(details) } => format!("{message}\n\n{details}"),
        CallbackPage::Failed { message, details: None } => message.clone(),
    }
}

impl Handler {
    async fn serve(self: Arc<Self>, mut socket: TcpStream) {
        let mut head = Vec::new();
        let mut buffer = [0u8; 2048];
        while !head.windows(4).any(|window| window == b"\r\n\r\n") && head.len() < 16 * 1024 {
            match socket.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => head.extend_from_slice(&buffer[..read]),
            }
        }
        let head = String::from_utf8_lossy(&head);
        let target = head.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("/");
        let (status, page) = self.handle(target);
        let (content_type, body) = match &self.render {
            Some(render) => ("text/html; charset=utf-8", render(&page)),
            None => ("text/plain; charset=utf-8", plain_text(&page)),
        };
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            _ => "Not Found",
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    }

    fn handle(&self, target: &str) -> (u16, CallbackPage) {
        let failed = |message: &str, details: Option<String>| CallbackPage::Failed { message: message.into(), details };
        let Ok(url) = Url::parse(&self.redirect_url).and_then(|base| base.join(target)) else {
            return (404, failed("Not found", None));
        };
        if !self.paths.iter().any(|path| path == url.path()) {
            return (404, failed("Not found", None));
        }
        let param = |name: &str| url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned());
        let state = param("state").filter(|state| !state.is_empty());
        let waiter = state.as_ref().and_then(|state| lock(&self.waiters).remove(state));
        let (Some(state), Some(waiter)) = (state, waiter) else {
            return (400, failed("Invalid or expired OAuth state", None));
        };
        if waiter.path.as_ref().is_some_and(|path| path != url.path()) {
            let error = OAuthError::other("The authorization response arrived on another redirect URI");
            let _ = waiter.sender.send(Err(error));
            return (400, failed("Unexpected redirect URI", None));
        }
        if let Some(error) = param("error").filter(|error| !error.is_empty()) {
            let description = param("error_description").unwrap_or(error);
            let _ = waiter.sender.send(Err(OAuthError::other(description.clone())));
            return (200, failed("Authorization failed. You may close this window.", Some(description)));
        }
        let Some(code) = param("code").filter(|code| !code.is_empty()) else {
            let _ = waiter.sender.send(Err(OAuthError::other("OAuth callback did not include an authorization code")));
            return (400, failed("Missing authorization code", None));
        };
        let iss = param("iss").filter(|iss| !iss.is_empty());
        let _ = waiter.sender.send(Ok(OAuthCallback { code, state, iss }));
        (200, CallbackPage::Ok)
    }
}
