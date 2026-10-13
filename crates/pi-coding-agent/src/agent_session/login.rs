//! Login and logout (upstream `ModelRuntime.login`/`logout` with the interactive provider
//! options): list providers with their auth methods, run an OAuth flow or ask for an API key,
//! and store the credential.

use pi_ai::auth::{
    ApiKeyCredential, AuthCheck, AuthError, AuthInteraction, AuthPrompt, AuthResult, AuthType, Credential,
    LoginOptions, UpdateFn,
};
use pi_ai::env::api_key_env_vars;
use tokio_util::sync::CancellationToken;

use super::ModelRuntime;

/// One login option: a provider with one auth method.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginProvider {
    pub id: String,
    pub name: String,
    pub auth_type: AuthType,
    /// Selector label of an OAuth login, for example "Sign in with ChatGPT".
    pub login_label: Option<String>,
    /// Whether the OAuth login is backed by a subscription.
    pub subscription: bool,
    /// How the provider is configured now, if it is.
    pub status: Option<AuthCheck>,
}

impl LoginProvider {
    /// "API key", "subscription", or "account".
    pub fn type_label(&self) -> &'static str {
        match self.auth_type {
            AuthType::ApiKey => "API key",
            AuthType::OAuth if self.subscription => "subscription",
            AuthType::OAuth => "account",
        }
    }
}

/// The text shown after a successful logout.
pub fn logout_message(provider: &LoginProvider) -> String {
    match provider.auth_type {
        AuthType::OAuth => format!("Logged out of {}", provider.name),
        AuthType::ApiKey => format!(
            "Removed stored API key for {}. Environment variables and models.json config are unchanged.",
            provider.name
        ),
    }
}

pub const NO_STORED_CREDENTIALS: &str = "No stored credentials to remove. /logout only removes credentials saved by /login; environment variables and models.json config are unchanged.";

impl ModelRuntime {
    fn provider_display_name(&self, id: &str) -> String {
        self.registry().provider_name(id).map_or_else(|| id.to_string(), str::to_string)
    }

    fn login_provider_ids(&self) -> Vec<String> {
        let mut ids = self.registry().providers();
        for id in pi_ai::auth::oauth::BUILTIN_OAUTH_PROVIDER_IDS {
            if !ids.iter().any(|known| known == id) {
                ids.push((*id).to_string());
            }
        }
        ids
    }

    fn supports_api_key_login(&self, id: &str) -> bool {
        !api_key_env_vars(id).is_empty()
            || super::login_flows::has_custom_api_key_login(id)
            || self.registry().auth_configs().contains_key(id)
    }

    /// Login options sorted by name; `auth_type` limits them to one method.
    pub async fn login_providers(&self, auth_type: Option<AuthType>) -> Vec<LoginProvider> {
        let mut options = Vec::new();
        for id in self.login_provider_ids() {
            let oauth = self.resolver().oauth(&id);
            let api_key = self.supports_api_key_login(&id);
            if oauth.is_none() && !api_key {
                continue;
            }
            let status = self.resolver().check(&id).await.ok().flatten();
            let name = self.provider_display_name(&id);
            let subscription = oauth.as_ref().is_some_and(|oauth| oauth.is_subscription());
            if let Some(oauth) = oauth.filter(|_| auth_type != Some(AuthType::ApiKey)) {
                options.push(LoginProvider {
                    id: id.clone(),
                    name: name.clone(),
                    auth_type: AuthType::OAuth,
                    login_label: oauth.login_label().map(str::to_string),
                    subscription,
                    status: status.clone(),
                });
            }
            if api_key && auth_type != Some(AuthType::OAuth) {
                let auth_type = AuthType::ApiKey;
                options.push(LoginProvider { id, name, auth_type, login_label: None, subscription, status });
            }
        }
        options.sort_by_key(|option| option.name.to_lowercase());
        options
    }

    /// Login options whose provider id or name equals `reference`, ignoring case.
    pub async fn find_login_providers(&self, reference: &str) -> Vec<LoginProvider> {
        let reference = reference.trim().to_lowercase();
        if reference.is_empty() {
            return Vec::new();
        }
        let options = self.login_providers(None).await;
        options
            .into_iter()
            .filter(|option| option.id.to_lowercase() == reference || option.name.to_lowercase() == reference)
            .collect()
    }

    /// Providers with a credential saved by `/login`, sorted by name.
    pub async fn logout_providers(&self) -> AuthResult<Vec<LoginProvider>> {
        let stored = self.credential_store().list().await?;
        let mut options: Vec<LoginProvider> = stored
            .into_iter()
            .map(|info| {
                let subscription =
                    self.resolver().oauth(&info.provider_id).is_some_and(|oauth| oauth.is_subscription());
                LoginProvider {
                    name: self.provider_display_name(&info.provider_id),
                    status: Some(AuthCheck { auth_type: info.auth_type, source: "stored credential".to_string() }),
                    id: info.provider_id,
                    auth_type: info.auth_type,
                    login_label: None,
                    subscription,
                }
            })
            .collect();
        options.sort_by_key(|option| option.name.to_lowercase());
        Ok(options)
    }

    /// Run the provider's OAuth flow or ask for an API key, then store the credential.
    pub async fn login(
        &self,
        provider: &str,
        auth_type: AuthType,
        interaction: &dyn AuthInteraction,
        options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<Credential> {
        if cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let name = self.provider_display_name(provider);
        let credential = match auth_type {
            AuthType::OAuth => {
                let oauth = self
                    .resolver()
                    .oauth(provider)
                    .ok_or_else(|| AuthError::other(format!("{name} does not support oauth login")))?;
                let login = oauth.login(interaction, options, cancel.clone());
                let credential = tokio::select! {
                    result = login => result?,
                    () = cancel.cancelled() => return Err(AuthError::Cancelled),
                };
                Credential::OAuth(credential)
            }
            AuthType::ApiKey => {
                if !self.supports_api_key_login(provider) {
                    return Err(AuthError::other(format!("{name} does not support api_key login")));
                }
                if let Some(credential) =
                    super::login_flows::custom_api_key_login(provider, interaction, &cancel).await?
                {
                    return self.store_login(provider, Credential::ApiKey(credential), &cancel).await;
                }
                let prompt = AuthPrompt::Secret { message: format!("Enter {name} API key"), placeholder: None };
                let key = interaction.prompt(prompt, cancel.clone()).await?;
                let key = key.trim().to_string();
                if key.is_empty() {
                    return Err(AuthError::other("API key cannot be empty"));
                }
                Credential::ApiKey(ApiKeyCredential { key: Some(key), env: None })
            }
        };
        self.store_login(provider, credential, &cancel).await
    }

    async fn store_login(
        &self,
        provider: &str,
        credential: Credential,
        cancel: &CancellationToken,
    ) -> AuthResult<Credential> {
        if cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let stored = credential.clone();
        let update: UpdateFn = Box::new(move |_| Box::pin(async move { Ok(Some(stored)) }));
        self.credential_store().update(provider, update).await?;
        self.refresh_configured_providers().await;
        Ok(credential)
    }

    /// Remove the stored credential of `provider`.
    pub async fn logout(&self, provider: &str) -> AuthResult<()> {
        self.credential_store().delete(provider).await?;
        self.refresh_configured_providers().await;
        Ok(())
    }
}
