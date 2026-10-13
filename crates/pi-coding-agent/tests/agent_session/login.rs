//! `/login` and `/logout` orchestration over the model runtime.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_ai::auth::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthResult, AuthType, Credential, InMemoryCredentialStore, LoginOptions,
    ModelAuth, OAuthAuth, OAuthCredential,
};
use pi_coding_agent::agent_session::login::logout_message;
use pi_coding_agent::agent_session::{ModelRuntime, ModelRuntimeOptions};
use pi_coding_agent::config::models_path;
use tokio_util::sync::CancellationToken;

use crate::harness::{Options, harness};

/// Answers every prompt with `answer` and records what it was asked.
struct ScriptedInteraction {
    answer: String,
    prompts: Mutex<Vec<AuthPrompt>>,
    events: Mutex<Vec<AuthEvent>>,
}

#[async_trait]
impl AuthInteraction for ScriptedInteraction {
    async fn prompt(&self, prompt: AuthPrompt, _cancel: CancellationToken) -> AuthResult<String> {
        self.prompts.lock().unwrap().push(prompt);
        Ok(self.answer.clone())
    }

    fn notify(&self, event: AuthEvent) {
        self.events.lock().unwrap().push(event);
    }
}

fn interaction(answer: &str) -> ScriptedInteraction {
    ScriptedInteraction { answer: answer.into(), prompts: Mutex::default(), events: Mutex::default() }
}

struct FakeOAuth;

#[async_trait]
impl OAuthAuth for FakeOAuth {
    fn name(&self) -> &str {
        "Local account"
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with Local")
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        _cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        interaction.notify(AuthEvent::Progress { message: "Waiting for the browser".into() });
        let extra = Default::default();
        Ok(OAuthCredential { refresh: "r".into(), access: "a".into(), expires: i64::MAX, extra })
    }

    async fn refresh(&self, credential: &OAuthCredential, _cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        Ok(credential.clone())
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}

#[tokio::test]
async fn logs_in_with_an_api_key_and_logs_out() {
    let h = harness(Options::default()).await;
    let runtime = h.session.model_runtime().clone();
    let options = runtime.find_login_providers("LOCAL").await;
    assert_eq!(options.len(), 1);
    assert_eq!((options[0].auth_type, options[0].type_label()), (AuthType::ApiKey, "API key"));
    assert!(runtime.login_providers(Some(AuthType::OAuth)).await.iter().any(|option| option.id == "anthropic"));

    let answers = interaction("  sk-entered  ");
    let credential =
        runtime.login("local", AuthType::ApiKey, &answers, &LoginOptions::default(), CancellationToken::new()).await;
    let Credential::ApiKey(stored) = credential.unwrap() else { panic!("api key credential") };
    assert_eq!(stored.key.as_deref(), Some("sk-entered"));
    let prompts = answers.prompts.lock().unwrap().clone();
    assert!(matches!(&prompts[0], AuthPrompt::Secret { message, .. } if message == "Enter local API key"));
    assert_eq!(runtime.api_key_for("local").await.as_deref(), Some("sk-entered"));

    let stored = runtime.logout_providers().await.unwrap();
    assert_eq!(stored.iter().map(|option| option.id.as_str()).collect::<Vec<_>>(), ["local"]);
    assert_eq!(
        logout_message(&stored[0]),
        "Removed stored API key for local. Environment variables and models.json config are unchanged."
    );
    runtime.logout("local").await.unwrap();
    assert!(runtime.logout_providers().await.unwrap().is_empty());
    assert_eq!(runtime.api_key_for("local").await.as_deref(), Some("test-key"), "models.json still applies");
}

#[tokio::test]
async fn logs_in_with_an_oauth_flow() {
    let h = harness(Options::default()).await;
    let runtime = ModelRuntime::create(ModelRuntimeOptions {
        models_path: models_path(&h.agent_dir),
        credentials: Arc::new(InMemoryCredentialStore::new(BTreeMap::new())),
        oauth: Some(Arc::new(|id: &str| (id == "local").then(|| Arc::new(FakeOAuth) as Arc<dyn OAuthAuth>))),
    })
    .await;
    let options = runtime.find_login_providers("local").await;
    let oauth = options.iter().find(|option| option.auth_type == AuthType::OAuth).unwrap();
    assert_eq!((oauth.login_label.as_deref(), oauth.type_label()), (Some("Sign in with Local"), "account"));

    let answers = interaction("");
    let cancel = CancellationToken::new();
    runtime.login("local", AuthType::OAuth, &answers, &LoginOptions::default(), cancel).await.unwrap();
    assert!(runtime.is_using_oauth("local").await);
    assert_eq!(runtime.api_key_for("local").await.as_deref(), Some("a"));
    assert_eq!(answers.events.lock().unwrap().len(), 1);
    let options = LoginOptions::default();
    let empty = runtime.login("local", AuthType::ApiKey, &answers, &options, CancellationToken::new()).await;
    assert_eq!(empty.unwrap_err().to_string(), "API key cannot be empty");
}

struct OutsideAuth;

#[async_trait]
impl pi_coding_agent::agent_session::ProviderAuth for OutsideAuth {
    async fn check(&self, _stored: Option<&Credential>) -> bool {
        false
    }

    async fn resolve(&self, _stored: Option<&Credential>) -> Result<Option<pi_ai::auth::ResolvedAuth>, String> {
        Ok(None)
    }
}

#[tokio::test]
async fn providers_configured_outside_pi_are_ambient_login_options() {
    let h = harness(Options::default()).await;
    let runtime = h.session.model_runtime().clone();
    runtime.register_provider_auth("outside-cloud", Some(Arc::new(OutsideAuth)));
    let options = runtime.find_login_providers("outside-cloud").await;
    assert_eq!(options.len(), 1, "{options:?}");
    assert_eq!((options[0].auth_type, options[0].ambient), (AuthType::ApiKey, true));
    let local = runtime.find_login_providers("local").await;
    assert!(local.iter().all(|option| !option.ambient));
    runtime.register_provider_auth("outside-cloud", None);
    assert!(runtime.find_login_providers("outside-cloud").await.is_empty());
}
