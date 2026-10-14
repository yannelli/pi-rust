//! Port of upstream `test/acme-server.test.ts`.

use pi_evals::evals::acme_server::{
    AcmeServer, OPENAI_MODEL_ID, OPENAI_PROBE_PROMPT, OPENAI_PROBE_RESPONSE, STREAM_MODEL_ID, STREAM_PROBE_PROMPT,
    STREAM_PROBE_RESPONSE, ServerMode,
};
use serde_json::{Value, json};

struct Fixture {
    mode: ServerMode,
    header: &'static str,
    authorized: &'static str,
    unauthorized: &'static str,
    model: &'static str,
    prompt: &'static str,
    response: &'static str,
    content_type: &'static str,
    unauthorized_message: &'static str,
    invalid_message: &'static str,
}

const FIXTURES: [Fixture; 2] = [
    Fixture {
        mode: ServerMode::OpenAi,
        header: "authorization",
        authorized: "Bearer resolved-acme-key",
        unauthorized: "Bearer wrong-key",
        model: OPENAI_MODEL_ID,
        prompt: OPENAI_PROBE_PROMPT,
        response: OPENAI_PROBE_RESPONSE,
        content_type: "text/event-stream",
        unauthorized_message: "Invalid Acme credential",
        invalid_message: "Invalid OpenAI-compatible request",
    },
    Fixture {
        mode: ServerMode::Stream,
        header: "x-acme-key",
        authorized: "resolved-stream-key",
        unauthorized: "wrong-key",
        model: STREAM_MODEL_ID,
        prompt: STREAM_PROBE_PROMPT,
        response: STREAM_PROBE_RESPONSE,
        content_type: "application/x-ndjson",
        unauthorized_message: "Invalid Acme Stream credential",
        invalid_message: "Invalid Acme Stream request",
    },
];

fn url(fixture: &Fixture, server: &AcmeServer) -> String {
    match fixture.mode {
        ServerMode::OpenAi => format!("{}/chat/completions", server.base_url()),
        ServerMode::Stream => format!("{}/generate", server.origin()),
    }
}

fn body(model: &str, prompt: &str, stream: bool) -> Value {
    json!({ "model": model, "messages": [{ "role": "user", "content": prompt }], "stream": stream })
}

async fn post(fixture: &Fixture, server: &AcmeServer, key: &str, payload: Value) -> reqwest::Response {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(url(fixture, server))
        .header("content-type", "application/json")
        .header(fixture.header, key)
        .body(payload.to_string())
        .send()
        .await
        .unwrap()
}

fn streamed_text(mode: ServerMode, raw: &str) -> String {
    let lines = raw.lines().filter(|line| !line.is_empty());
    match mode {
        ServerMode::Stream => lines
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["type"] == "text_delta")
            .map(|event| event["text"].as_str().unwrap_or_default().to_string())
            .collect(),
        ServerMode::OpenAi => lines
            .filter(|line| line.starts_with("data: ") && *line != "data: [DONE]")
            .map(|line| serde_json::from_str::<Value>(&line["data: ".len()..]).unwrap())
            .map(|chunk| chunk["choices"][0]["delta"]["content"].as_str().unwrap_or_default().to_string())
            .collect(),
    }
}

async fn started(fixture: &Fixture) -> AcmeServer {
    let server = AcmeServer::new(fixture.mode);
    server.start().await.unwrap();
    server.reset();
    server
}

#[tokio::test]
async fn accepts_the_probe_request_and_records_it() {
    for fixture in &FIXTURES {
        let server = started(fixture).await;
        let response = post(fixture, &server, fixture.authorized, body(fixture.model, fixture.prompt, true)).await;
        assert_eq!(response.status(), 200);
        let content_type = response.headers()["content-type"].to_str().unwrap().to_string();
        assert!(content_type.contains(fixture.content_type));
        assert_eq!(streamed_text(fixture.mode, &response.text().await.unwrap()), fixture.response);
        assert!(server.valid_request_received());
        server.stop();
    }
}

#[tokio::test]
async fn rejects_a_bad_credential_with_401_and_does_not_record_a_probe() {
    for fixture in &FIXTURES {
        let server = started(fixture).await;
        let response = post(fixture, &server, fixture.unauthorized, body(fixture.model, fixture.prompt, true)).await;
        assert_eq!(response.status(), 401);
        assert_eq!(response.text().await.unwrap(), fixture.unauthorized_message);
        assert!(!server.valid_request_received());
    }
}

#[tokio::test]
async fn rejects_a_malformed_request_with_422_and_does_not_record_a_probe() {
    for fixture in &FIXTURES {
        let server = started(fixture).await;
        let response = post(fixture, &server, fixture.authorized, body(fixture.model, fixture.prompt, false)).await;
        assert_eq!(response.status(), 422);
        assert_eq!(response.text().await.unwrap(), fixture.invalid_message);
        assert!(!server.valid_request_received());
    }
}

#[tokio::test]
async fn does_not_treat_a_non_probe_success_as_the_probe() {
    for fixture in &FIXTURES {
        let server = started(fixture).await;
        let response = post(fixture, &server, fixture.authorized, body(fixture.model, "hello", true)).await;
        assert_eq!(response.status(), 200);
        assert!(!server.valid_request_received());
    }
}

#[tokio::test]
async fn clears_the_probe_flag_on_reset() {
    for fixture in &FIXTURES {
        let server = started(fixture).await;
        post(fixture, &server, fixture.authorized, body(fixture.model, fixture.prompt, true)).await;
        assert!(server.valid_request_received());
        server.reset();
        assert!(!server.valid_request_received());
    }
}
