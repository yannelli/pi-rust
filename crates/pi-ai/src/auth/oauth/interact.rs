//! Prompt helpers that also honor the login's cancellation token.

use tokio_util::sync::CancellationToken;

use crate::auth::{AuthError, AuthInteraction, AuthPrompt, AuthResult, PromptOption};

/// Shows `prompt` with a token tied to the login; fails with [`AuthError::Cancelled`]
/// when the login is cancelled even if the UI ignores the token.
pub(crate) async fn prompt(
    interaction: &dyn AuthInteraction,
    prompt: AuthPrompt,
    cancel: &CancellationToken,
) -> AuthResult<String> {
    let token = cancel.child_token();
    let _dismiss = token.clone().drop_guard();
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(AuthError::Cancelled),
        answer = interaction.prompt(prompt, token) => answer,
    }
}

/// A `select` prompt over `(id, label)` options; returns the chosen id.
pub(crate) async fn select(
    interaction: &dyn AuthInteraction,
    message: &str,
    options: &[(&str, &str)],
    cancel: &CancellationToken,
) -> AuthResult<String> {
    let options = options
        .iter()
        .map(|(id, label)| PromptOption { id: (*id).into(), label: (*label).into(), description: None })
        .collect();
    prompt(interaction, AuthPrompt::Select { message: message.into(), options }, cancel).await
}
