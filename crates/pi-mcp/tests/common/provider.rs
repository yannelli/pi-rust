//! An OAuth client provider that keeps everything in memory, like the one in oauth.test.ts.

use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use pi_mcp::OAuthError;
use pi_mcp::oauth::{
    InvalidateKind, OAuthClientInformation, OAuthClientMetadata, OAuthClientProvider, OAuthDiscoveryState, OAuthTokens,
};
use url::Url;

#[derive(Default)]
pub struct Saved {
    pub client: Option<OAuthClientInformation>,
    pub tokens: Option<OAuthTokens>,
    pub verifier: Option<String>,
    pub discovery: Option<OAuthDiscoveryState>,
    pub authorization_url: Option<Url>,
}

pub struct TestProvider {
    redirect_url: String,
    saved: Mutex<Saved>,
}

impl TestProvider {
    pub fn new(redirect_url: &str) -> Arc<Self> {
        Arc::new(Self { redirect_url: redirect_url.into(), saved: Mutex::default() })
    }

    pub fn saved(&self) -> MutexGuard<'_, Saved> {
        self.saved.lock().unwrap()
    }

    pub fn authorization_param(&self, name: &str) -> Option<String> {
        let url = self.saved().authorization_url.clone()?;
        url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
    }
}

#[async_trait]
impl OAuthClientProvider for TestProvider {
    fn redirect_url(&self) -> String {
        self.redirect_url.clone()
    }

    fn client_metadata(&self) -> OAuthClientMetadata {
        OAuthClientMetadata {
            redirect_uris: vec![self.redirect_url.clone()],
            client_name: Some("pi-mcp-test".into()),
            grant_types: Some(vec!["authorization_code".into(), "refresh_token".into()]),
            response_types: Some(vec!["code".into()]),
            token_endpoint_auth_method: Some("none".into()),
            ..OAuthClientMetadata::default()
        }
    }

    async fn state(&self) -> Result<Option<String>, OAuthError> {
        Ok(Some("expected-state".into()))
    }

    async fn client_information(&self) -> Result<Option<OAuthClientInformation>, OAuthError> {
        Ok(self.saved().client.clone())
    }

    async fn save_client_information(&self, information: OAuthClientInformation) -> Result<(), OAuthError> {
        self.saved().client = Some(information);
        Ok(())
    }

    async fn tokens(&self) -> Result<Option<OAuthTokens>, OAuthError> {
        Ok(self.saved().tokens.clone())
    }

    async fn save_tokens(&self, tokens: OAuthTokens) -> Result<(), OAuthError> {
        self.saved().tokens = Some(tokens);
        Ok(())
    }

    async fn redirect_to_authorization(&self, url: Url) -> Result<(), OAuthError> {
        self.saved().authorization_url = Some(url);
        Ok(())
    }

    async fn save_code_verifier(&self, verifier: String) -> Result<(), OAuthError> {
        self.saved().verifier = Some(verifier);
        Ok(())
    }

    async fn code_verifier(&self) -> Result<String, OAuthError> {
        self.saved().verifier.clone().ok_or_else(|| OAuthError::other("Missing code verifier"))
    }

    async fn invalidate_credentials(&self, kind: InvalidateKind) -> Result<(), OAuthError> {
        let mut saved = self.saved();
        let all = kind == InvalidateKind::All;
        if all || kind == InvalidateKind::Client {
            saved.client = None;
        }
        if all || kind == InvalidateKind::Tokens {
            saved.tokens = None;
        }
        if all || kind == InvalidateKind::Verifier {
            saved.verifier = None;
        }
        if all || kind == InvalidateKind::Discovery {
            saved.discovery = None;
        }
        Ok(())
    }

    async fn save_discovery_state(&self, state: OAuthDiscoveryState) -> Result<(), OAuthError> {
        self.saved().discovery = Some(state);
        Ok(())
    }

    async fn discovery_state(&self) -> Result<Option<OAuthDiscoveryState>, OAuthError> {
        Ok(self.saved().discovery.clone())
    }
}
