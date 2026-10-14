//! Port of upstream `radius-auth.ts`: explicit or stored Radius credentials for relay connections.

use std::path::PathBuf;
use std::sync::Arc;

use pi_ai::auth::AuthOverrides;
use pi_ai::auth::oauth::normalize_radius_gateway_url;
use pi_chord::{ChordError, ChordResult};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::agent_session::{ModelRuntime, ModelRuntimeOptions};
pub use crate::diagnostics::bug_upload::ENV_RADIUS_GATEWAY;
use crate::diagnostics::bug_upload::radius_gateway_url;

/// Upstream `AuthInput` of the experimental command options: a token, or a file holding one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RadiusAuthInput {
    Token(String),
    File(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RadiusRelayAuth {
    pub gateway: String,
    pub token: String,
}

/// Resolve explicit or stored Radius credentials anew for each relay connection.
pub struct RadiusRelayAuthResolver {
    input: Option<RadiusAuthInput>,
    gateway: String,
    agent_dir: PathBuf,
    model_runtime: OnceCell<Arc<ModelRuntime>>,
}

fn cancelled(signal: Option<&CancellationToken>) -> ChordResult<()> {
    match signal {
        Some(signal) if signal.is_cancelled() => Err(ChordError::error("The operation was aborted")),
        _ => Ok(()),
    }
}

impl RadiusRelayAuthResolver {
    /// `gateway` defaults to the Radius gateway, honoring `PI_RADIUS_GATEWAY`.
    pub fn new(input: Option<RadiusAuthInput>, gateway: Option<&str>) -> Self {
        let gateway = gateway.map(normalize_radius_gateway_url).unwrap_or_else(radius_gateway_url);
        Self { input, gateway, agent_dir: crate::config::agent_dir(), model_runtime: OnceCell::new() }
    }

    pub fn gateway(&self) -> &str {
        &self.gateway
    }

    pub async fn resolve(
        &self,
        required: bool,
        signal: Option<&CancellationToken>,
    ) -> ChordResult<Option<RadiusRelayAuth>> {
        cancelled(signal)?;
        if pi_ai::env::process_env_value("PI_OFFLINE").is_some() {
            if required {
                return Err(ChordError::error("Radius relay connections are unavailable in offline mode"));
            }
            return Ok(None);
        }
        if let Some(token) = self.explicit_token().await? {
            return Ok(Some(RadiusRelayAuth { gateway: self.gateway.clone(), token }));
        }
        let runtime = self
            .model_runtime
            .get_or_init(|| async {
                Arc::new(ModelRuntime::create(ModelRuntimeOptions::for_agent_dir(&self.agent_dir)).await)
            })
            .await;
        cancelled(signal)?;
        let overrides = AuthOverrides { min_oauth_validity_ms: Some(5 * 60_000), ..AuthOverrides::default() };
        let resolved = runtime.resolver().resolve("radius", &overrides).await.ok().flatten();
        let token = resolved.and_then(|resolved| resolved.auth.api_key).filter(|token| !token.is_empty());
        if let Some(token) = token {
            return Ok(Some(RadiusRelayAuth { gateway: self.gateway.clone(), token }));
        }
        if required {
            return Err(ChordError::error(
                "Radius authentication is required; start Pi and run /login radius, then retry",
            ));
        }
        Ok(None)
    }

    async fn explicit_token(&self) -> ChordResult<Option<String>> {
        let value = match &self.input {
            None => return Ok(None),
            Some(RadiusAuthInput::Token(token)) => token.clone(),
            Some(RadiusAuthInput::File(path)) => {
                let path = std::path::absolute(crate::config::expand_tilde(path)).map_err(ChordError::other)?;
                tokio::fs::read_to_string(path).await.map_err(ChordError::other)?
            }
        };
        let token = value.trim();
        if token.is_empty() {
            return Err(ChordError::error("Radius authentication token must not be empty"));
        }
        Ok(Some(token.to_string()))
    }
}
