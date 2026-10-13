//! Loopback OAuth redirect handler shared by the browser sign-in flows.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use super::loopback::{LoopbackHandler, LoopbackRequest, LoopbackResponse, LoopbackServer, loopback_uri};
use super::page::{oauth_error_html, oauth_success_html};
use crate::auth::{AuthError, AuthInteraction, AuthPrompt, AuthResult};

/// Finishes sign-in with the received code before the browser page is sent, so the page
/// can show exchange failures. Use `|code| async move { Ok(code) }.boxed()` to exchange later.
pub type CompleteFn<T> = Arc<dyn Fn(String) -> BoxFuture<'static, AuthResult<T>> + Send + Sync>;

pub struct CallbackServerOptions<T> {
    /// Provider name used on the browser page, for example `OpenAI`.
    pub provider_name: String,
    pub host: String,
    /// Port to listen on; `0` picks a free port.
    pub port: u16,
    pub path: String,
    /// Host in the redirect URI when it differs from `host`, for example `localhost`.
    pub redirect_host: Option<String>,
    /// Expected `state` parameter; `None` when the provider sends none.
    pub state: Option<String>,
    pub complete: CompleteFn<T>,
    pub cancel: Option<CancellationToken>,
    pub timeout: Option<Duration>,
}

struct Shared<T> {
    provider_name: String,
    claimed: bool,
    result: Option<AuthResult<Option<T>>>,
}

type SharedState<T> = Arc<(Mutex<Shared<T>>, CancellationToken)>;

fn settle<T>(shared: &mut Shared<T>, settled: &CancellationToken, result: AuthResult<Option<T>>) {
    if !settled.is_cancelled() {
        shared.result = Some(result);
        settled.cancel();
    }
}

fn finish<T>(state: &SharedState<T>, result: AuthResult<Option<T>>) {
    let (shared, settled) = &**state;
    settle(&mut shared.lock().expect("callback state lock"), settled, result);
}

/// A listening callback server. Dropping it closes it.
pub struct CallbackServer<T> {
    redirect_uri: String,
    state: SharedState<T>,
    server: LoopbackServer,
}

impl<T: Send + 'static> CallbackServer<T> {
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// The result of `complete`, or `None` after [`cancel`](Self::cancel). Fails when the provider
    /// redirects with an error, `complete` fails, the login is cancelled, or the timeout elapses.
    pub async fn wait(&self) -> AuthResult<Option<T>> {
        let (shared, settled) = &*self.state;
        settled.cancelled().await;
        let result = shared.lock().expect("callback state lock").result.take();
        result.unwrap_or_else(|| Err(AuthError::other("OAuth callback result was already taken")))
    }

    /// Stop waiting for the browser unless a callback is already being completed.
    pub fn cancel(&self) {
        let (shared, settled) = &*self.state;
        let mut shared = shared.lock().expect("callback state lock");
        if !shared.claimed {
            settle(&mut shared, settled, Ok(None));
        }
    }

    pub fn close(&self) {
        finish(&self.state, Err(AuthError::other("OAuth callback server closed")));
        self.server.close();
    }
}

impl<T> Drop for CallbackServer<T> {
    fn drop(&mut self) {
        finish(&self.state, Err(AuthError::other("OAuth callback server closed")));
    }
}

pub async fn start_callback_server<T: Send + 'static>(
    options: CallbackServerOptions<T>,
) -> AuthResult<CallbackServer<T>> {
    let cancel = options.cancel.clone().unwrap_or_default();
    if cancel.is_cancelled() {
        return Err(AuthError::Cancelled);
    }
    let shared = Shared { provider_name: options.provider_name.clone(), claimed: false, result: None };
    let state: SharedState<T> = Arc::new((Mutex::new(shared), CancellationToken::new()));
    let handler = callback_handler(&options, state.clone());
    let server = LoopbackServer::bind(&options.host, options.port, handler)
        .await
        .map_err(|error| AuthError::other(format!("OAuth callback server could not listen: {error}")))?;

    let watchdog = state.clone();
    let (provider_name, timeout) = (options.provider_name.clone(), options.timeout);
    tokio::spawn(async move {
        let settled = watchdog.1.clone();
        let timer = async {
            match timeout {
                Some(timeout) => tokio::time::sleep(timeout).await,
                None => futures::future::pending().await,
            }
        };
        tokio::select! {
            _ = settled.cancelled() => {}
            _ = cancel.cancelled() => finish(&watchdog, Err(AuthError::Cancelled)),
            _ = timer => finish(&watchdog, Err(AuthError::other(format!("{provider_name} sign-in timed out")))),
        }
    });

    let redirect_host = options.redirect_host.as_deref().unwrap_or(&options.host);
    let redirect_uri = loopback_uri(redirect_host, server.port, &options.path);
    Ok(CallbackServer { redirect_uri, state, server })
}

fn callback_handler<T: Send + 'static>(options: &CallbackServerOptions<T>, state: SharedState<T>) -> LoopbackHandler {
    let (path, expected_state, complete) = (options.path.clone(), options.state.clone(), options.complete.clone());
    Arc::new(move |request: LoopbackRequest| {
        let (path, expected_state, complete, state) =
            (path.clone(), expected_state.clone(), complete.clone(), state.clone());
        async move { handle_callback(request, &path, expected_state.as_deref(), complete, state).await }.boxed()
    })
}

async fn handle_callback<T>(
    request: LoopbackRequest,
    path: &str,
    expected_state: Option<&str>,
    complete: CompleteFn<T>,
    state: SharedState<T>,
) -> LoopbackResponse {
    let page = LoopbackResponse::page;
    let param = |name: &str| super::http::url_param(&request.url, name);
    if request.method != "GET" || request.url.path() != path {
        return page(404, oauth_error_html("Callback route not found.", None));
    }
    if let Some(expected) = expected_state
        && param("state").as_deref() != Some(expected)
    {
        return page(400, oauth_error_html("State mismatch.", None));
    }
    let provider_name = {
        let mut shared = state.0.lock().expect("callback state lock");
        if shared.claimed || state.1.is_cancelled() {
            return page(409, oauth_error_html("This sign-in has already been handled.", None));
        }
        if let Some(error) = param("error").filter(|error| !error.is_empty()) {
            let description = param("error_description").unwrap_or(error);
            let name = shared.provider_name.clone();
            let failure = AuthError::other(format!("{name} authorization failed: {description}"));
            settle(&mut shared, &state.1, Err(failure));
            return page(400, oauth_error_html(&format!("{name} authorization failed."), Some(&description)));
        }
        if param("code").is_none_or(|code| code.is_empty()) {
            return page(400, oauth_error_html("Missing authorization code.", None));
        }
        shared.claimed = true;
        shared.provider_name.clone()
    };
    let code = param("code").unwrap_or_default();
    match complete(code).await {
        Ok(value) => {
            let message = format!("Signed in to {provider_name}. You may now close this page.");
            let response = page(200, oauth_success_html(&message));
            finish(&state, Ok(Some(value)));
            response
        }
        Err(error) => {
            let response =
                page(502, oauth_error_html(&format!("{provider_name} sign-in failed."), Some(&error.to_string())));
            finish(&state, Err(error));
            response
        }
    }
}

/// Which path finished the browser step.
#[derive(Debug, PartialEq, Eq)]
pub enum CallbackOrManual<T> {
    Callback(T),
    Manual(String),
}

/// Waits for the browser callback, or for the user to paste the code or redirect URL when the
/// browser cannot reach the loopback server (for example over SSH). Without a callback server
/// only the manual prompt is used. The prompt's token is cancelled once either path wins
/// or `cancel` (the login's token) fires.
pub async fn wait_for_callback_or_manual_input<T: Send + 'static>(
    interaction: &dyn AuthInteraction,
    callback: Option<&CallbackServer<T>>,
    message: &str,
    placeholder: &str,
    cancel: &CancellationToken,
) -> AuthResult<CallbackOrManual<T>> {
    match race(interaction, callback, message, placeholder, cancel).await {
        // The prompt shares the login's token, so its own error can arrive first.
        Err(_) if cancel.is_cancelled() => Err(AuthError::Cancelled),
        result => result,
    }
}

async fn race<T: Send + 'static>(
    interaction: &dyn AuthInteraction,
    callback: Option<&CallbackServer<T>>,
    message: &str,
    placeholder: &str,
    cancel: &CancellationToken,
) -> AuthResult<CallbackOrManual<T>> {
    let manual_cancel = cancel.child_token();
    let _dismiss = manual_cancel.clone().drop_guard();
    let prompt = AuthPrompt::ManualCode { message: message.into(), placeholder: Some(placeholder.into()) };
    let mut manual = interaction.prompt(prompt, manual_cancel.clone());
    let Some(callback) = callback else {
        return tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(AuthError::Cancelled),
            input = manual => input.map(CallbackOrManual::Manual),
        };
    };

    let mut manual_result: Option<AuthResult<String>> = None;
    let waited = {
        let mut wait = std::pin::pin!(callback.wait());
        loop {
            tokio::select! {
                input = &mut manual, if manual_result.is_none() => {
                    callback.cancel();
                    manual_result = Some(input);
                }
                waited = &mut wait => break waited,
            }
        }
    };
    let value = waited?;
    if let Some(Err(error)) = manual_result {
        return Err(error);
    }
    if let Some(value) = value {
        return Ok(CallbackOrManual::Callback(value));
    }
    let input = match manual_result {
        Some(input) => input?,
        None => manual.await?,
    };
    Ok(CallbackOrManual::Manual(input))
}
