//! Port of upstream `test/configured-runtime.test.ts`.

use std::collections::BTreeMap;

use pi_evals::evals::acme_server::{
    AcmeServer, OPENAI_MODEL_ID, OPENAI_PROBE_PROMPT, OPENAI_PROBE_RESPONSE, OPENAI_PROVIDER_ID, ServerMode,
};
use pi_evals::evals::configured_runtime::{
    ProviderScenario, inspect_added_model, inspect_provider, load_configured_model_runtime,
};
use pi_evals::fsutil::json_equal;
use serde_json::{Value, json};
use tempfile::TempDir;

fn model() -> Value {
    json!({
        "id": OPENAI_MODEL_ID, "name": "Acme Chat", "provider": OPENAI_PROVIDER_ID, "reasoning": false,
        "input": ["text"], "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 32768, "maxTokens": 4096,
    })
}

fn agent_dir_with(models: &str) -> TempDir {
    let directory = tempfile::Builder::new().prefix("pi-eval-provider-probe-").tempdir().unwrap();
    std::fs::write(directory.path().join("models.json"), models).unwrap();
    directory
}

fn acme_models_json(server: &AcmeServer) -> String {
    let model = model();
    json!({
        "providers": {
            OPENAI_PROVIDER_ID: {
                "baseUrl": server.base_url(), "api": "openai-completions", "apiKey": "$ACME_API_KEY",
                "models": [{
                    "id": model["id"], "name": model["name"], "reasoning": model["reasoning"], "input": model["input"],
                    "cost": model["cost"], "contextWindow": model["contextWindow"], "maxTokens": model["maxTokens"],
                }],
            },
        },
    })
    .to_string()
}

fn probe(server: &AcmeServer, content: &str, api_key: &str) -> ProviderScenario {
    ProviderScenario {
        provider_id: OPENAI_PROVIDER_ID.into(),
        model_id: OPENAI_MODEL_ID.into(),
        prompt: content.into(),
        env: BTreeMap::from([("ACME_API_KEY".to_string(), api_key.to_string())]),
        max_tokens: 32,
        valid_request_received: server.probe(),
    }
}

async fn started() -> AcmeServer {
    let server = AcmeServer::new(ServerMode::OpenAi);
    server.start().await.unwrap();
    server.reset();
    server
}

fn assert_json(actual: &Value, expected: &Value) {
    assert!(json_equal(actual, expected), "actual: {actual:#}\nexpected: {expected:#}");
}

#[tokio::test]
async fn probes_a_models_json_provider_through_the_model_runtime() {
    let server = started().await;
    let directory = agent_dir_with(&acme_models_json(&server));
    let runtime = load_configured_model_runtime(directory.path()).await;
    let output = inspect_provider(&runtime, &probe(&server, OPENAI_PROBE_PROMPT, "resolved-acme-key")).await;
    let response = json!({ "text": OPENAI_PROBE_RESPONSE, "stopReason": "stop", "inputTokens": 3, "outputTokens": 2 });
    assert_json(
        &output,
        &json!({ "result": { "validRequestReceived": true, "model": model(), "response": response } }),
    );
}

#[tokio::test]
async fn keeps_a_non_probe_completion_distinct_from_a_valid_probe() {
    let server = started().await;
    let directory = agent_dir_with(&acme_models_json(&server));
    let runtime = load_configured_model_runtime(directory.path()).await;
    let output = inspect_provider(&runtime, &probe(&server, "hello", "resolved-acme-key")).await;
    let response = json!({ "text": OPENAI_PROBE_RESPONSE, "stopReason": "stop", "inputTokens": 3, "outputTokens": 2 });
    assert_json(
        &output,
        &json!({ "result": { "validRequestReceived": false, "model": model(), "response": response } }),
    );
}

#[tokio::test]
async fn returns_a_structured_error_when_the_configured_model_is_missing() {
    let server = started().await;
    let directory = agent_dir_with(r#"{"providers":{}}"#);
    let runtime = load_configured_model_runtime(directory.path()).await;
    let output = inspect_provider(&runtime, &probe(&server, OPENAI_PROBE_PROMPT, "resolved-acme-key")).await;
    let error = format!("Model {OPENAI_PROVIDER_ID}/{OPENAI_MODEL_ID} is unavailable after reload.");
    assert_json(&output, &json!({ "result": { "error": error } }));
    assert!(!server.valid_request_received());
}

#[tokio::test]
async fn returns_a_structured_error_when_models_json_cannot_be_parsed() {
    let server = started().await;
    let directory = agent_dir_with("{");
    let runtime = load_configured_model_runtime(directory.path()).await;
    let output = inspect_provider(&runtime, &probe(&server, OPENAI_PROBE_PROMPT, "resolved-acme-key")).await;
    assert!(output["result"]["error"].as_str().unwrap().contains("Failed to parse models.json"), "{output}");
    assert!(!server.valid_request_received());
}

#[tokio::test]
async fn does_not_treat_an_unauthorized_completion_as_a_valid_probe() {
    let server = started().await;
    let directory = agent_dir_with(&acme_models_json(&server));
    let runtime = load_configured_model_runtime(directory.path()).await;
    let output = inspect_provider(&runtime, &probe(&server, OPENAI_PROBE_PROMPT, "wrong-key")).await;
    let response = json!({ "text": "", "stopReason": "error", "inputTokens": 0, "outputTokens": 0 });
    assert_json(
        &output,
        &json!({ "result": { "validRequestReceived": false, "model": model(), "response": response } }),
    );
}

const PROVIDER: &str = "openai";
const MODEL_ID: &str = "fixture-chat";

fn added_model() -> Value {
    json!({
        "id": MODEL_ID, "name": "Fixture Chat", "provider": PROVIDER, "reasoning": true, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }, "contextWindow": 32768, "maxTokens": 4096,
    })
}

#[tokio::test]
async fn loads_an_added_model_without_dropping_built_in_models() {
    let model = added_model();
    let models = json!({
        "providers": { PROVIDER: { "models": [{
            "id": model["id"], "name": model["name"], "reasoning": model["reasoning"], "input": model["input"],
            "cost": model["cost"], "contextWindow": model["contextWindow"], "maxTokens": model["maxTokens"],
        }] } },
    });
    let directory = agent_dir_with(&models.to_string());
    let runtime = load_configured_model_runtime(directory.path()).await;
    let output = inspect_added_model(&runtime, PROVIDER, MODEL_ID).await;
    assert_json(&output, &json!({ "result": { "model": model, "existingModelsPreserved": true } }));
}

#[tokio::test]
async fn returns_a_structured_error_when_the_added_model_is_missing() {
    let directory = agent_dir_with(r#"{"providers":{}}"#);
    let runtime = load_configured_model_runtime(directory.path()).await;
    let output = inspect_added_model(&runtime, PROVIDER, MODEL_ID).await;
    let error = format!("Model {PROVIDER}/{MODEL_ID} is unavailable after reload.");
    assert_json(&output, &json!({ "result": { "error": error } }));
}

#[tokio::test]
async fn returns_a_structured_error_when_added_models_json_cannot_be_parsed() {
    let directory = agent_dir_with("{");
    let runtime = load_configured_model_runtime(directory.path()).await;
    let output = inspect_added_model(&runtime, PROVIDER, MODEL_ID).await;
    assert!(output["result"]["error"].as_str().unwrap().contains("Failed to parse models.json"), "{output}");
}
