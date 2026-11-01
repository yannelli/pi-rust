use std::collections::BTreeMap;
use std::sync::Arc;

use pi_ai::auth::{Credential, InMemoryCredentialStore, OAuthCredential};
use serde_json::json;

use super::request_auth;
use crate::agent_session::model_runtime::{ModelRuntime, ModelRuntimeOptions};

#[tokio::test]
async fn kimi_login_region_preserves_an_explicit_models_json_endpoint() {
    for configured in [None, Some("https://proxy.example/kimi/v1")] {
        let dir = tempfile::tempdir().unwrap();
        let models_path = dir.path().join("models.json");
        if let Some(base_url) = configured {
            std::fs::write(&models_path, json!({"providers": {"kimi-coding": {"baseUrl": base_url}}}).to_string())
                .unwrap();
        }
        let credential = OAuthCredential {
            access: "fixture-access".into(),
            refresh: "fixture-refresh".into(),
            expires: i64::MAX,
            extra: json!({"oauthHost": "https://auth.kimi.ai", "baseUrl": "https://api.kimi.ai/coding/v1"})
                .as_object()
                .unwrap()
                .clone(),
        };
        let credentials = Arc::new(InMemoryCredentialStore::new(BTreeMap::from([(
            "kimi-coding".into(),
            Credential::OAuth(credential),
        )])));
        let runtime = ModelRuntime::create(ModelRuntimeOptions {
            models_path,
            credentials,
            oauth: Some(Arc::new(pi_ai::auth::oauth::builtin_oauth)),
            credential_env: Arc::new(|_: &str| None),
        })
        .await;
        assert_eq!(runtime.load_error(), None);
        let model = runtime.get_model("kimi-coding", "kimi-for-coding").unwrap();
        let request = request_auth(&runtime, &model).await.unwrap();
        assert_eq!(request.model.base_url, configured.unwrap_or("https://api.kimi.ai/coding/v1"));
        assert_eq!(request.auth.headers["Authorization"], "Bearer fixture-access");
    }
}
