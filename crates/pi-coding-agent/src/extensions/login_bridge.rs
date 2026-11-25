//! JavaScript login flows driven from Rust: callbacks of a running login reach the login's
//! `AuthInteraction` through `oauth.callback` and `oauth.notify`, answered in call order.

use std::future::Future;
use std::sync::Arc;

use pi_ai::auth::{AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthResult};
use pi_js::{HostValue, JsError};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::ops::{Args, error};
use super::runner::ExtensionRunner;
use crate::agent_session::lock;

/// A login callback from JavaScript, answered by the login's interaction.
pub(crate) struct LoginRequest {
    name: String,
    args: Vec<Value>,
    reply: oneshot::Sender<Result<Value, String>>,
}

/// Run the JavaScript login `start(key, signal)` until it settles, answering its callbacks
/// with `interaction`. `key` names the login in callbacks; `signal` aborts with `cancel`.
pub(crate) async fn drive_login<F>(
    runner: &Arc<ExtensionRunner>,
    interaction: &dyn AuthInteraction,
    cancel: &CancellationToken,
    start: impl FnOnce(u64, u64) -> F,
) -> AuthResult<Value>
where
    F: Future<Output = AuthResult<Value>>,
{
    let (sender, mut requests) = mpsc::unbounded_channel::<LoginRequest>();
    let key = runner.next_id();
    lock(&runner.logins).insert(key, sender);
    let signal = runner.signal_for(cancel);
    let login = start(key, signal);
    tokio::pin!(login);
    let result = loop {
        tokio::select! {
            result = &mut login => break result,
            Some(request) = requests.recv() => {
                let reply = answer(runner, interaction, &request, cancel).await;
                let _ = request.reply.send(reply);
            }
        }
    };
    lock(&runner.logins).remove(&key);
    runner.release_signal(signal);
    if cancel.is_cancelled() {
        return Err(AuthError::Cancelled);
    }
    result
}

async fn answer(
    runner: &ExtensionRunner,
    interaction: &dyn AuthInteraction,
    request: &LoginRequest,
    cancel: &CancellationToken,
) -> Result<Value, String> {
    let arg = request.args.first().cloned().unwrap_or(Value::Null);
    let text = |key: &str| arg[key].as_str().map(str::to_string);
    let prompt = match request.name.as_str() {
        // Upstream `AuthInteraction` of native providers: `prompt(prompt)` with an optional
        // signal id of the prompt's own `signal`, and `notify(event)`.
        "notify" => {
            let event = serde_json::from_value(arg).map_err(|failure| format!("Invalid login event: {failure}"))?;
            interaction.notify(event);
            return Ok(Value::Null);
        }
        "prompt" => {
            let prompt: AuthPrompt =
                serde_json::from_value(arg).map_err(|failure| format!("Invalid login prompt: {failure}"))?;
            let own = request.args.get(1).and_then(Value::as_u64).map(|id| runner.token(Some(id)));
            let prompt_cancel = cancel.child_token();
            if let Some(own) = own {
                let linked = prompt_cancel.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        () = own.cancelled() => linked.cancel(),
                        () = linked.cancelled() => {}
                    }
                });
            }
            let answer = interaction.prompt(prompt, prompt_cancel.clone()).await;
            prompt_cancel.cancel();
            return answer.map(Value::from).map_err(|failure| failure.to_string());
        }
        "onAuth" => {
            let url = text("url").unwrap_or_default();
            interaction.notify(AuthEvent::AuthUrl { url, instructions: text("instructions") });
            return Ok(Value::Null);
        }
        "onDeviceCode" => {
            interaction.notify(AuthEvent::DeviceCode {
                user_code: text("userCode").unwrap_or_default(),
                verification_uri: text("verificationUri").unwrap_or_default(),
                open_browser_on_confirm: arg["openBrowserOnConfirm"].as_bool(),
                copy_code_on_confirm: arg["copyCodeOnConfirm"].as_bool(),
                interval_seconds: arg["intervalSeconds"].as_u64(),
                expires_in_seconds: arg["expiresInSeconds"].as_u64(),
            });
            return Ok(Value::Null);
        }
        "onProgress" => {
            let message = arg.as_str().map(str::to_string).unwrap_or_default();
            interaction.notify(AuthEvent::Progress { message });
            return Ok(Value::Null);
        }
        "onPrompt" => {
            AuthPrompt::Text { message: text("message").unwrap_or_default(), placeholder: text("placeholder") }
        }
        "onSelect" => AuthPrompt::Select {
            message: text("message").unwrap_or_default(),
            options: serde_json::from_value(arg["options"].clone()).unwrap_or_default(),
        },
        _ => AuthPrompt::ManualCode { message: "Paste the authorization code".into(), placeholder: None },
    };
    interaction.prompt(prompt, cancel.child_token()).await.map(Value::from).map_err(|failure| failure.to_string())
}

/// Queues a login callback in call order; the receiver gets its answer.
fn queue_login_callback(
    runner: &ExtensionRunner,
    args: &Args,
) -> Result<oneshot::Receiver<Result<Value, String>>, JsError> {
    let key = args.json(0)["key"].as_u64().unwrap_or(0);
    let name = args.str(1)?;
    let rest = args.0.iter().skip(2).cloned().map(HostValue::into_json).collect();
    let sender = lock(&runner.logins).get(&key).cloned().ok_or_else(|| error("The login has finished"))?;
    let (reply, answer) = oneshot::channel();
    sender.send(LoginRequest { name, args: rest, reply }).map_err(|_| error("The login has finished"))?;
    Ok(answer)
}

/// `oauth.notify`: a login callback with no answer, such as `onAuth` or `onProgress`.
pub(crate) fn oauth_notify(runner: &ExtensionRunner, args: &Args) -> Result<Value, JsError> {
    queue_login_callback(runner, args).map(|_| Value::Null)
}

/// `oauth.callback`: a login callback forwarded to the waiting login.
pub(crate) async fn oauth_callback(runner: &Arc<ExtensionRunner>, args: &Args) -> Result<Value, JsError> {
    let answer = queue_login_callback(runner, args)?;
    answer.await.map_err(|_| error("The login has finished"))?.map_err(error)
}
