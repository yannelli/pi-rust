use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use super::*;
use crate::auth::store::InMemoryCredentialStore;
use crate::auth::{AuthInteraction, LoginOptions};

struct CountingOAuth {
    refreshes: AtomicUsize,
    lifetime_ms: i64,
}

#[async_trait]
impl OAuthAuth for CountingOAuth {
    fn name(&self) -> &str {
        "Test OAuth"
    }

    async fn login(
        &self,
        _interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        _cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        Err(AuthError::Cancelled)
    }

    async fn refresh(&self, credential: &OAuthCredential, _cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let count = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(OAuthCredential {
            access: format!("fresh-{count}"),
            expires: now_ms() + self.lifetime_ms,
            ..credential.clone()
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}

fn store(entries: Vec<(&str, Credential)>) -> Arc<dyn CredentialStore> {
    Arc::new(InMemoryCredentialStore::new(
        entries.into_iter().map(|(id, credential)| (id.to_string(), credential)).collect(),
    ))
}

fn key(value: &str) -> Credential {
    Credential::ApiKey(ApiKeyCredential { key: Some(value.into()), env: None })
}

fn env(pairs: &[(&str, &str)]) -> AuthOverrides {
    AuthOverrides {
        env: Some(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()),
        ..Default::default()
    }
}

async fn api_key(resolver: &AuthResolver, provider: &str, overrides: &AuthOverrides) -> Option<String> {
    resolver.resolve(provider, overrides).await.unwrap().and_then(|resolved| resolved.auth.api_key)
}

#[tokio::test]
async fn priority_is_runtime_key_then_stored_then_models_json_then_environment() {
    let resolver = AuthResolver::without_oauth(store(vec![("groq", key("stored"))]));
    let mut configs = HashMap::new();
    configs.insert("groq".into(), ProviderAuthConfig { api_key: Some("configured".into()), ..Default::default() });
    configs.insert(
        "cerebras".into(),
        ProviderAuthConfig { api_key: Some("$PI_TEST_CEREBRAS".into()), ..Default::default() },
    );
    resolver.set_provider_configs(configs);
    let groq_env = env(&[("GROQ_API_KEY", "from-env")]);
    let runtime = AuthOverrides { api_key: Some("runtime".into()), ..groq_env.clone() };
    assert_eq!(api_key(&resolver, "groq", &runtime).await.as_deref(), Some("runtime"));
    assert_eq!(api_key(&resolver, "groq", &groq_env).await.as_deref(), Some("stored"));
    let cerebras = env(&[("PI_TEST_CEREBRAS", "templated"), ("CEREBRAS_API_KEY", "env")]);
    let resolved = resolver.resolve("cerebras", &cerebras).await.unwrap().unwrap();
    assert_eq!(
        (resolved.auth.api_key.as_deref(), resolved.source.as_deref()),
        (Some("templated"), Some("configured API key"))
    );
    let xai = env(&[("XAI_API_KEY", "xai-env")]);
    let resolved = resolver.resolve("xai", &xai).await.unwrap().unwrap();
    assert_eq!((resolved.auth.api_key.as_deref(), resolved.source.as_deref()), (Some("xai-env"), Some("XAI_API_KEY")));
    assert!(resolver.resolve("custom-without-key", &AuthOverrides::default()).await.unwrap().is_none());
    let error = resolver.resolve("cerebras", &AuthOverrides::default()).await;
    assert!(matches!(error, Err(AuthError::Other(message)) if message.contains("PI_TEST_CEREBRAS")));
}

#[tokio::test]
async fn anthropic_auth_token_is_sent_as_a_bearer_header() {
    let resolver = AuthResolver::without_oauth(store(vec![]));
    let overrides = env(&[(env::ANTHROPIC_AUTH_TOKEN_ENV, "tok"), (env::ANTHROPIC_API_KEY_ENV, "sk-ant")]);
    let resolved = resolver.resolve("anthropic", &overrides).await.unwrap().unwrap();
    assert_eq!(resolved.auth.api_key, None);
    assert_eq!(resolved.auth.headers["Authorization"], "Bearer tok");
    let federation = env(&[
        (env::ANTHROPIC_FEDERATION_RULE_ID_ENV, "rule"),
        (env::ANTHROPIC_ORGANIZATION_ID_ENV, "org"),
        (env::ANTHROPIC_IDENTITY_TOKEN_FILE_ENV, "/token"),
    ]);
    if std::env::var(env::ANTHROPIC_API_KEY_ENV).is_err() && std::env::var(env::ANTHROPIC_OAUTH_TOKEN_ENV).is_err() {
        let resolved = resolver.resolve("anthropic", &federation).await.unwrap().unwrap();
        assert_eq!(resolved.env.unwrap()["ANTHROPIC_ORGANIZATION_ID"], "org");
    }
}

#[tokio::test]
async fn models_json_headers_resolve_templates_and_auth_header() {
    let resolver = AuthResolver::without_oauth(store(vec![]));
    let mut headers = BTreeMap::new();
    headers.insert("X-Team".to_string(), "${PI_TEST_TEAM}".to_string());
    let mut model_headers = BTreeMap::new();
    model_headers.insert("m1".to_string(), BTreeMap::from([("X-Model".to_string(), "literal".to_string())]));
    let config = ProviderAuthConfig { api_key: Some("sk-local".into()), headers, auth_header: true, model_headers };
    resolver.set_provider_configs(HashMap::from([("local".to_string(), config)]));
    let overrides = env(&[("PI_TEST_TEAM", "core")]);
    let resolved = resolver.resolve_for_model("local", "m1", &overrides).await.unwrap().unwrap();
    assert_eq!(resolved.auth.headers["X-Team"], "core");
    assert_eq!(resolved.auth.headers["X-Model"], "literal");
    assert_eq!(resolved.auth.headers["Authorization"], "Bearer sk-local");
}

#[tokio::test(flavor = "multi_thread")]
async fn expiring_oauth_tokens_refresh_once_and_persist() {
    let expired = OAuthCredential { refresh: "r".into(), access: "old".into(), expires: 0, ..Default::default() };
    let credentials = store(vec![("acme", Credential::OAuth(expired))]);
    let oauth = Arc::new(CountingOAuth { refreshes: AtomicUsize::new(0), lifetime_ms: 3_600_000 });
    let lookup_oauth = oauth.clone();
    let resolver = Arc::new(AuthResolver::new(
        credentials.clone(),
        Arc::new(move |id| (id == "acme").then(|| lookup_oauth.clone() as Arc<dyn OAuthAuth>)),
    ));
    let tasks: Vec<_> = (0..4)
        .map(|_| {
            let resolver = resolver.clone();
            tokio::spawn(async move { api_key(&resolver, "acme", &AuthOverrides::default()).await })
        })
        .collect();
    for task in tasks {
        assert_eq!(task.await.unwrap().as_deref(), Some("fresh-1"));
    }
    assert_eq!(oauth.refreshes.load(Ordering::SeqCst), 1);
    let Some(Credential::OAuth(saved)) = credentials.read("acme").await.unwrap() else { panic!("oauth") };
    assert_eq!(saved.access, "fresh-1");
    assert_eq!(resolver.check("acme").await.unwrap().unwrap().auth_type, AuthType::OAuth);

    let too_short = AuthOverrides { min_oauth_validity_ms: Some(2 * 3_600_000), ..Default::default() };
    let error = resolver.resolve("acme", &too_short).await.unwrap_err();
    assert!(error.to_string().contains("expires too soon"), "{error}");

    credentials.delete("acme").await.unwrap();
    let logged_out = refresh_stored_oauth(&credentials, "acme", oauth.clone(), 0).await.unwrap();
    assert!(logged_out.is_none());
}

#[tokio::test]
async fn check_reports_configuration_without_running_commands() {
    let stored_oauth = Credential::OAuth(OAuthCredential {
        refresh: "r".into(),
        access: "a".into(),
        expires: 1,
        ..Default::default()
    });
    let resolver = AuthResolver::without_oauth(store(vec![("no-handler", stored_oauth), ("openai", key("sk"))]));
    let configs = HashMap::from([
        ("cmd".to_string(), ProviderAuthConfig { api_key: Some("!exit 1".into()), ..Default::default() }),
        (
            "unset".to_string(),
            ProviderAuthConfig { api_key: Some("$PI_RUST_UNSET_KEY_VAR".into()), ..Default::default() },
        ),
    ]);
    resolver.set_provider_configs(configs);
    assert_eq!(resolver.check("openai").await.unwrap().unwrap().source, "stored credential");
    assert_eq!(resolver.check("cmd").await.unwrap().unwrap().source, "configured API key");
    assert!(resolver.check("unset").await.unwrap().is_none());
    assert!(resolver.check("no-handler").await.unwrap().is_none());
    assert!(resolver.check("custom-without-key").await.unwrap().is_none());
}
