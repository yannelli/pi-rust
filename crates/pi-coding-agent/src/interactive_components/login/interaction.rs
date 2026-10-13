//! Drives a [`LoginDialog`] from a login flow: an [`AuthInteraction`] that forwards prompts and
//! events to the UI thread, and [`run_dialog_login`], which logs in and stores the credential.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use async_trait::async_trait;
use pi_ai::auth::{AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthResult, AuthType, Credential, LoginOptions};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::LoginDialog;
use super::llama_cpp::{LLAMA_CPP_PROVIDER_ID, llama_cpp_login};
use crate::agent_session::ModelRuntime;

/// Upstream `RADIUS_LOGIN_INTRO`, shown under Radius select prompts.
pub const RADIUS_LOGIN_INTRO: &str = "Radius is a service crafted for Pi by the builders of Pi, Earendil Works";

enum UiRequest {
    Prompt(AuthPrompt, CancellationToken, oneshot::Sender<AuthResult<String>>),
    Notify(AuthEvent),
}

/// An [`AuthInteraction`] (`Send + Sync`) whose prompts and events show in a [`LoginDialog`].
/// Create it with [`dialog_interaction`] on the UI thread inside a `LocalSet`.
pub struct DialogInteraction {
    tx: mpsc::UnboundedSender<UiRequest>,
}

#[async_trait]
impl AuthInteraction for DialogInteraction {
    async fn prompt(&self, prompt: AuthPrompt, cancel: CancellationToken) -> AuthResult<String> {
        if cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let (reply, receiver) = oneshot::channel();
        self.tx.send(UiRequest::Prompt(prompt, cancel.clone(), reply)).map_err(|_| AuthError::Cancelled)?;
        tokio::select! {
            result = receiver => result.unwrap_or(Err(AuthError::Cancelled)),
            () = cancel.cancelled() => Err(AuthError::Cancelled),
        }
    }

    fn notify(&self, event: AuthEvent) {
        let _ = self.tx.send(UiRequest::Notify(event));
    }
}

/// Routes an event to the dialog like upstream `notifyAuthDialog`.
pub fn notify_dialog(dialog: &mut LoginDialog, event: &AuthEvent) {
    match event {
        AuthEvent::AuthUrl { url, instructions } => dialog.show_auth(url, instructions.as_deref()),
        AuthEvent::DeviceCode { user_code, verification_uri, .. } => {
            dialog.show_device_code(user_code, verification_uri);
            dialog.show_waiting("Waiting for authentication...");
        }
        AuthEvent::Info { message, links } => dialog.show_info(message, links, false),
        AuthEvent::Progress { message } => dialog.show_progress(message),
    }
}

fn start_prompt(
    dialog: &Rc<RefCell<LoginDialog>>,
    prompt: AuthPrompt,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = AuthResult<String>>>> {
    let mut dialog = dialog.borrow_mut();
    match prompt {
        AuthPrompt::Select { message, options } => {
            let labels: Vec<String> = options.iter().map(|option| option.label.clone()).collect();
            let description = (dialog.provider_id() == "radius").then_some(RADIUS_LOGIN_INTRO);
            let chosen = dialog.show_select(&message, labels, description);
            Box::pin(async move {
                let label = chosen.await?;
                options
                    .into_iter()
                    .find(|option| option.label == label)
                    .map(|option| option.id)
                    .ok_or(AuthError::Cancelled)
            })
        }
        AuthPrompt::ManualCode { message, .. } => Box::pin(dialog.show_manual_input(&message)),
        AuthPrompt::Text { message, placeholder } | AuthPrompt::Secret { message, placeholder } => {
            Box::pin(dialog.show_prompt(&message, placeholder.as_deref()))
        }
    }
}

async fn pump(dialog: Weak<RefCell<LoginDialog>>, mut rx: mpsc::UnboundedReceiver<UiRequest>) {
    while let Some(request) = rx.recv().await {
        let Some(dialog) = dialog.upgrade() else { break };
        match request {
            UiRequest::Notify(event) => notify_dialog(&mut dialog.borrow_mut(), &event),
            UiRequest::Prompt(prompt, cancel, reply) => {
                let response = start_prompt(&dialog, prompt);
                tokio::task::spawn_local(async move {
                    let result = tokio::select! {
                        result = response => result,
                        () = cancel.cancelled() => Err(AuthError::Cancelled),
                    };
                    let _ = reply.send(result);
                });
            }
        }
    }
}

/// An interaction bound to `dialog`. Spawns the UI-side task with `spawn_local`; it ends when
/// the interaction is dropped.
pub fn dialog_interaction(dialog: &Rc<RefCell<LoginDialog>>) -> DialogInteraction {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::task::spawn_local(pump(Rc::downgrade(dialog), rx));
    DialogInteraction { tx }
}

/// Logs in to `provider_id` through the dialog (upstream `loginProvider`) and stores the
/// credential: OAuth flows and API keys through [`ModelRuntime::login`], and the llama.cpp
/// server URL login for `llama.cpp`. The dialog's cancel key aborts the flow.
pub async fn run_dialog_login(
    dialog: &Rc<RefCell<LoginDialog>>,
    runtime: &ModelRuntime,
    provider_id: &str,
    auth_type: AuthType,
    options: &LoginOptions,
) -> AuthResult<Credential> {
    let interaction = dialog_interaction(dialog);
    let cancel = dialog.borrow().cancel_token();
    if provider_id == LLAMA_CPP_PROVIDER_ID && auth_type == AuthType::ApiKey {
        let credential = tokio::select! {
            result = llama_cpp_login(&interaction, cancel.clone()) => result?,
            () = cancel.cancelled() => return Err(AuthError::Cancelled),
        };
        let credential = Credential::ApiKey(credential);
        let stored = credential.clone();
        let update: pi_ai::auth::UpdateFn = Box::new(move |_| Box::pin(async move { Ok(Some(stored)) }));
        runtime.credential_store().update(provider_id, update).await?;
        runtime.refresh_configured_providers().await;
        return Ok(credential);
    }
    runtime.login(provider_id, auth_type, &interaction, options, cancel).await
}
