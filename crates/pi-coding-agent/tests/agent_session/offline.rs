//! The harness fails fast instead of falling back to a built-in model that would reach a real
//! provider with credentials from the environment.

use std::collections::BTreeMap;

use pi_ai::auth::{ApiKeyCredential, Credential};
use pi_ai::providers::faux::FauxResponse;
use serde_json::json;

use crate::harness::{Options, harness};
use crate::offline_guard::{assert_offline_model, is_offline};

#[tokio::test]
#[should_panic(expected = "the test models.json did not load")]
async fn a_models_json_that_does_not_load_fails_the_harness() {
    // A cost without `cacheRead` and `cacheWrite` is invalid, so the whole file is ignored.
    let options = Options {
        tiny: json!({"cost": {"input": 3, "output": 15}}),
        responses: vec![FauxResponse::text("never sent")],
        ..Options::default()
    };
    harness(options).await;
}

#[tokio::test]
#[should_panic(expected = "instead of the faux test model")]
async fn a_session_on_a_builtin_model_fails_the_harness() {
    let key = Credential::ApiKey(ApiKeyCredential { key: Some("sk-test".into()), env: None });
    let options = Options {
        settings: json!({"defaultProvider": "anthropic", "defaultModel": "claude-sonnet-4-5"}),
        credentials: BTreeMap::from([("anthropic".to_string(), key)]),
        ..Options::default()
    };
    harness(options).await;
}

#[tokio::test]
async fn the_default_harness_runs_on_the_faux_model() {
    let h = harness(Options { responses: vec![FauxResponse::text("hi")], ..Options::default() }).await;
    let model = h.session.model().unwrap();
    assert!(model.api.starts_with("faux-session-") && is_offline(&model), "{model:?}");
    h.session.prompt("hello", Vec::new()).await.unwrap();
    assert_eq!(h.requests().len(), 1);
}

#[test]
fn builtin_models_are_not_offline_and_loopback_servers_are() {
    let mut model = pi_ai::models::builtin_models().iter().find(|model| model.provider == "openai").unwrap().clone();
    assert!(!is_offline(&model), "{}", model.base_url);
    for base_url in ["http://127.0.0.1:9/v1", "http://localhost:8080", "http://[::1]:1234/v1"] {
        model.base_url = base_url.into();
        assert!(is_offline(&model), "{base_url}");
    }
    model.base_url = "http://unused".into();
    assert!(!is_offline(&model));
    model.api = "faux-session-1".into();
    assert!(is_offline(&model));
}

#[test]
#[should_panic(expected = "is not served by a faux provider or a loopback server")]
fn a_builtin_model_fails_the_offline_check() {
    let model = pi_ai::models::builtin_models().iter().find(|model| model.provider == "anthropic").unwrap();
    assert_offline_model(Some(model));
}
