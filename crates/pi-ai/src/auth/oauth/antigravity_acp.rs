//! Google Antigravity login through the installed Antigravity ACP server, which runs the Google
//! sign-in and keeps the credential. pi stores only a marker that the login happened.

use async_trait::async_trait;
use serde_json::Map;
use tokio_util::sync::CancellationToken;

use super::http::now_ms;
use super::interact;
use crate::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthResult, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
};
use crate::providers::antigravity_acp as acp;

/// The request key of an Antigravity login; the server holds the real credential.
pub const ANTIGRAVITY_ACP_MARKER: &str = "antigravity-acp";
const MARKER_LIFETIME_MS: i64 = 365 * 24 * 3600 * 1000;

#[derive(Default)]
pub struct AntigravityAcpOAuth;

/// Asks to install Google's Antigravity ACP server, then installs it.
async fn offer_install(interaction: &dyn AuthInteraction, cancel: &CancellationToken) -> AuthResult<()> {
    let Some(release) = acp::release() else {
        return Err(AuthError::other("Google publishes no Antigravity ACP server for this platform"));
    };
    let message = format!(
        "pi needs Google's Antigravity ACP server. Install version {} ({} MB from dl.google.com)?",
        release.version,
        release.bytes / 1_000_000
    );
    let choice =
        interact::select(interaction, &message, &[("install", "Install"), ("cancel", "Cancel")], cancel).await?;
    if choice != "install" {
        return Err(AuthError::other(acp::NOT_INSTALLED));
    }
    let progress = |message: String| interaction.notify(AuthEvent::Progress { message });
    let root = acp::install_root().map_err(AuthError::Other)?;
    let path = acp::install(&root, &release, &progress, cancel).await.map_err(AuthError::Other)?;
    progress(format!("Installed the Antigravity ACP server at {}", path.display()));
    Ok(())
}

fn marker() -> OAuthCredential {
    let access = ANTIGRAVITY_ACP_MARKER.to_string();
    OAuthCredential { refresh: access.clone(), access, expires: now_ms() + MARKER_LIFETIME_MS, extra: Map::new() }
}

#[async_trait]
impl OAuthAuth for AntigravityAcpOAuth {
    fn name(&self) -> &str {
        "Google Antigravity"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with Google (Antigravity ACP)")
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        if !acp::installed() {
            offer_install(interaction, &cancel).await?;
        }
        interaction.notify(AuthEvent::Progress { message: "Starting the Antigravity ACP server".into() });
        match acp::login(interaction, &cancel).await {
            Ok(()) => Ok(marker()),
            Err(_) if cancel.is_cancelled() => Err(AuthError::Cancelled),
            Err(error) => Err(AuthError::Other(error)),
        }
    }

    async fn refresh(&self, _credential: &OAuthCredential, _cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        Ok(marker())
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}
