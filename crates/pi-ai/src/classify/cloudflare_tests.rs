//! Port of upstream `cloudflare-workers-ai-system-one.test.ts`, through the `classify` entry point.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::test_support::{answers_json, classifier_model, context, with_key};
use super::{ClassifierContext, ClassifierOptions, ClassifierStopReason, classify};
use crate::providers::mock_server::{MockResponse, MockServer};
use crate::types::Model;

fn sample_context() -> ClassifierContext {
    context(json!({
        "state": { "message": "Help! My payouts have been failing for 3 days." },
        "questions": {
            "is_urgent": {
                "type": "bool",
                "instructions": "Does this convey urgency?",
                "criteria": { "true": "Explicitly time-sensitive", "false": "No urgency expressed" },
            },
            "department": {
                "type": "choice",
                "instructions": "Which team should handle this?",
                "criteria": { "billing": "Payments", "technical": "Bugs" },
            },
        },
    }))
}

/// Model output from the Workers AI `typesafe/jev` documentation.
fn jev_output() -> Value {
    json!({
        "model": "jev-1.13.0",
        "answers": {
            "is_urgent": { "type": "noul", "noul": 0.95 },
            "department": { "type": "choice", "choice": "billing", "confidence": 0.8, "probabilities": { "billing": 0.87, "technical": 0.13 } },
        },
        "usage": { "input_tokens": 426, "output_tokens": 73 },
    })
}

/// REST envelope recorded from the live `/ai/run` endpoint.
fn rest_response(state: &str, result: Value) -> Value {
    json!({
        "result": { "state": state, "result": result, "gatewayMetadata": { "keySource": "Unified" } },
        "success": true,
        "errors": [],
        "messages": [],
    })
}

/// A model on the account-scoped REST root, as the catalog lists it.
fn model(server: &MockServer, id: &str) -> Model {
    let base_url = format!("{}/client/v4/accounts/{{CLOUDFLARE_ACCOUNT_ID}}/ai", server.base_url);
    classifier_model("cloudflare-workers-ai-system-one", "cloudflare-workers-ai", id, &base_url)
}

fn auth() -> ClassifierOptions {
    let mut options = with_key("cf-key");
    options.request.env = BTreeMap::from([("CLOUDFLARE_ACCOUNT_ID".to_string(), "account-id".to_string())]);
    options
}

#[tokio::test]
async fn runs_jev_through_the_account_scoped_run_endpoint() {
    let mut server = MockServer::start(MockResponse::json(&rest_response("Completed", jev_output()))).await;
    let context = sample_context();
    let result = classify(&model(&server, "typesafe/jev"), &context, auth()).await;

    let request = server.request().await;
    assert_eq!(request.path, "/client/v4/accounts/account-id/ai/run");
    assert_eq!(request.body["model"], "typesafe/jev");
    assert_eq!(request.body["input"]["state"], Value::Object(context.state.clone()));
    assert_eq!(request.body["input"]["questions"]["is_urgent"]["type"], "noul");
    assert_eq!(request.body["input"]["questions"]["department"]["type"], "choice");
    assert_eq!(request.headers["authorization"], "Bearer cf-key");
    assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
    let answers = answers_json(&result);
    assert_eq!(answers["is_urgent"], json!({ "type": "bool", "probability": 0.95 }));
    let department = &answers["department"];
    assert_eq!((department["choice"].as_str(), department["confidence"].as_f64()), (Some("billing"), Some(0.8)));
    let usage = result.usage.expect("usage");
    assert_eq!((usage.input, usage.output, usage.total_tokens), (426, 73, 499));
}

#[tokio::test]
async fn runs_cloudflare_hosted_models_and_parses_their_direct_output() {
    // Output recorded from a live `/ai/run` call; the envelope has no run record.
    let clef_output = json!({
        "model": "clef",
        "answers": {
            "is_urgent": { "type": "noul", "noul": 0.9912 },
            "department": { "type": "choice", "choice": "technical", "probabilities": { "billing": 0.1632, "technical": 0.8368 }, "confidence": 0.4538 },
        },
        "usage": { "input_tokens": 222, "output_tokens": 0 },
    });
    let models =
        [("@cf/cloudflare/clef", 0.24), ("@cf/cloudflare/clef-flash", 0.038), ("@cf/cloudflare/clef-omni", 0.15)];
    for (id, input_price) in models {
        let response = json!({ "result": clef_output, "success": true, "errors": [], "messages": [] });
        let mut server = MockServer::start(MockResponse::json(&response)).await;
        let mut model = model(&server, id);
        model.cost.input = input_price;
        let result = classify(&model, &sample_context(), auth()).await;

        let request = server.request().await;
        assert_eq!(request.path, "/client/v4/accounts/account-id/ai/run");
        assert_eq!(request.body["model"], id);
        assert_eq!(request.body["input"]["questions"]["is_urgent"]["type"], "noul");
        assert_eq!(result.stop_reason, ClassifierStopReason::Stop);
        let answers = answers_json(&result);
        assert_eq!(answers["is_urgent"], json!({ "type": "bool", "probability": 0.9912 }));
        assert_eq!(answers["department"]["choice"], "technical");
        let usage = result.usage.expect("usage");
        assert_eq!((usage.input, usage.output, usage.total_tokens), (222, 0, 222));
        assert!((usage.cost.input - 222.0 * input_price / 1_000_000.0).abs() < 1e-12);
    }
}

#[tokio::test]
async fn reports_runs_that_did_not_complete_and_envelope_errors() {
    let failed = json!({ "success": false, "errors": [{ "code": 5007, "message": "No such model" }], "result": null });
    let server = MockServer::sequence(vec![
        MockResponse::json(&rest_response("Queued", Value::Null)),
        MockResponse::json(&failed),
    ])
    .await;
    let model = model(&server, "typesafe/jev");
    let queued = classify(&model, &sample_context(), auth()).await;
    let rejected = classify(&model, &sample_context(), auth()).await;

    assert_eq!(queued.stop_reason, ClassifierStopReason::Error);
    assert!(queued.error_message.unwrap().contains("run did not complete (state: Queued)"));
    assert_eq!(rejected.stop_reason, ClassifierStopReason::Error);
    assert!(rejected.error_message.unwrap().contains("Cloudflare Workers AI error: No such model"));
}
