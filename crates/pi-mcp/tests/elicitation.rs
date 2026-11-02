//! `elicitation/create` from an in-memory server: the capability, the answers of the callback,
//! content validation, URL mode, and completion notifications.

mod common;

use std::sync::{Arc, Mutex};

use common::{TestServer, create_server, wait_until};
use futures::FutureExt;
use pi_mcp::runtime::{ConnectionOptions, InMemoryAuthStorageBackend, McpOAuthCredentialStore, McpServerConnection};
use pi_mcp::{
    ElicitFn, ElicitationAction, ElicitationMode, ElicitationRequest, ElicitationResponse, JsonRpcMessage,
    McpElicitation, McpServerConfig, McpServerEntry, McpServerLog, SharedTransport,
};
use serde_json::{Value, json};

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "age": { "type": "integer" },
            "color": { "type": "string", "enum": ["red", "blue"] }
        },
        "required": ["name"]
    })
}

struct Setup {
    connection: McpServerConnection,
    server: TestServer,
    seen: Arc<Mutex<Vec<ElicitationRequest>>>,
    completed: Arc<Mutex<Vec<String>>>,
    log: Arc<McpServerLog>,
    _dir: tempfile::TempDir,
}

/// A connected server whose elicitations `answer` answers.
async fn setup(answer: impl Fn(&ElicitationRequest) -> ElicitationResponse + Send + Sync + 'static) -> Setup {
    let (transport, server) = create_server().await;
    server.set_result("tools/list", json!({ "tools": [] }));
    let seen: Arc<Mutex<Vec<ElicitationRequest>>> = Arc::default();
    let completed: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = seen.clone();
    let elicit: ElicitFn = Arc::new(move |request, _| {
        let response = answer(&request);
        sink.lock().unwrap().push(request);
        async move { response }.boxed()
    });
    let done = completed.clone();
    let mut elicitation = McpElicitation::new(elicit);
    elicitation.on_complete = Some(Arc::new(move |server, id| done.lock().unwrap().push(format!("{server}:{id}"))));
    let config: McpServerConfig = serde_json::from_value(json!({ "command": "unused" })).unwrap();
    let credentials = McpOAuthCredentialStore::with_backend(Arc::new(InMemoryAuthStorageBackend::default()), None);
    let mut options = ConnectionOptions::new(
        McpServerEntry::new("forms", config, "test"),
        std::env::current_dir().unwrap(),
        credentials,
    );
    let slot = Mutex::new(Some(transport));
    options.create_transport = Arc::new(move |_, _, _| Ok(slot.lock().unwrap().take().unwrap() as SharedTransport));
    options.elicitation = Some(elicitation);
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(McpServerLog::new(dir.path().join("mcp.log")));
    options.log = Some(log.clone());
    let connection = McpServerConnection::new(options);
    connection.get_client().await.unwrap();
    Setup { connection, server, seen, completed, log, _dir: dir }
}

/// Send `elicitation/create` and wait for the client's answer.
async fn elicit(server: &TestServer, id: &str, params: Value) -> Result<Value, (i64, String)> {
    server.send(JsonRpcMessage::request(id, "elicitation/create", Some(params))).await;
    let mut answer = None;
    wait_until(|| {
        answer = server.messages().into_iter().find_map(|message| match message {
            JsonRpcMessage::Response { id: answered, result } if answered == id.into() => Some(result),
            _ => None,
        });
        answer.is_some()
    })
    .await;
    answer.unwrap().map_err(|error| (error.code, error.message))
}

fn form(message: &str) -> Value {
    json!({ "mode": "form", "message": message, "requestedSchema": schema() })
}

#[tokio::test]
async fn announces_form_and_url_elicitation() {
    let s = setup(|_| ElicitationResponse::decline()).await;
    let initialize = s.server.messages()[0].to_value();
    assert_eq!(initialize["params"]["capabilities"]["elicitation"], json!({ "form": {}, "url": {} }));
    s.connection.close().await;
}

#[tokio::test]
async fn answers_accept_decline_and_cancel_from_the_callback() {
    let s = setup(|request| match request.message.as_str() {
        "accept" => {
            let content = json!({ "name": "Ada", "age": 36, "color": "red" });
            ElicitationResponse::accept(content.as_object().cloned())
        }
        // Content of a decline is not sent.
        "decline" => {
            ElicitationResponse { action: ElicitationAction::Decline, content: json!({"x": 1}).as_object().cloned() }
        }
        _ => ElicitationResponse::cancel(),
    })
    .await;
    assert_eq!(
        elicit(&s.server, "a", form("accept")).await,
        Ok(json!({ "action": "accept", "content": { "name": "Ada", "age": 36, "color": "red" } }))
    );
    assert_eq!(elicit(&s.server, "d", form("decline")).await, Ok(json!({ "action": "decline" })));
    assert_eq!(elicit(&s.server, "c", form("cancel")).await, Ok(json!({ "action": "cancel" })));
    let seen = s.seen.lock().unwrap().clone();
    assert_eq!(seen[0].server, "forms");
    assert_eq!(seen[0].mode, ElicitationMode::Form);
    assert_eq!(seen[0].requested_schema, Some(schema()));
    s.connection.close().await;
}

#[tokio::test]
async fn declines_content_that_does_not_match_the_schema() {
    let s = setup(|request| {
        let content = match request.message.as_str() {
            "type" => json!({ "name": "Ada", "age": "old" }),
            "enum" => json!({ "name": "Ada", "color": "green" }),
            "missing" => json!({ "age": 3 }),
            _ => json!({ "name": "Ada", "extra": true }),
        };
        ElicitationResponse::accept(content.as_object().cloned())
    })
    .await;
    for message in ["type", "enum", "missing", "extra"] {
        assert_eq!(elicit(&s.server, message, form(message)).await, Ok(json!({ "action": "decline" })), "{message}");
    }
    let log = std::fs::read_to_string(s.log.path()).unwrap();
    assert!(log.contains("[forms] warning pi: elicitation answer declined: \"age\" is not a valid integer"), "{log}");
    assert!(log.contains("\"color\" is not a valid string") && log.contains("\"name\" is required"), "{log}");
    s.connection.close().await;
}

#[tokio::test]
async fn handles_url_mode_and_its_completion_notification() {
    let s = setup(|_| ElicitationResponse::accept(json!({ "ignored": 1 }).as_object().cloned())).await;
    let params =
        json!({ "mode": "url", "message": "Sign in", "url": "https://example.com/connect", "elicitationId": "e1" });
    assert_eq!(elicit(&s.server, "u", params).await, Ok(json!({ "action": "accept" })));
    let seen = s.seen.lock().unwrap()[0].clone();
    assert_eq!(
        (seen.mode, seen.url.as_deref(), seen.elicitation_id.as_deref()),
        (ElicitationMode::Url, Some("https://example.com/connect"), Some("e1"))
    );
    // Unknown ids are ignored; a known id completes once.
    for id in ["other", "e1", "e1"] {
        let params = json!({ "elicitationId": id });
        s.server.send(JsonRpcMessage::notification("notifications/elicitation/complete", Some(params))).await;
    }
    wait_until(|| !s.completed.lock().unwrap().is_empty()).await;
    common::settle().await;
    assert_eq!(*s.completed.lock().unwrap(), ["forms:e1"]);

    let missing = json!({ "mode": "url", "message": "x", "url": "not a url", "elicitationId": "e2" });
    assert_eq!(elicit(&s.server, "bad", missing).await.unwrap_err().0, -32602);
    let unknown = json!({ "mode": "telepathy", "message": "x" });
    assert_eq!(
        elicit(&s.server, "mode", unknown).await.unwrap_err(),
        (-32602, "Unsupported elicitation mode: telepathy".into())
    );
    s.connection.close().await;
}

#[tokio::test]
async fn treats_requests_without_a_mode_as_form_mode() {
    let s = setup(|_| ElicitationResponse::accept(json!({ "name": "x" }).as_object().cloned())).await;
    let params = json!({ "message": "Name?", "requestedSchema": schema() });
    assert_eq!(elicit(&s.server, "f", params).await, Ok(json!({ "action": "accept", "content": { "name": "x" } })));
    assert_eq!(s.seen.lock().unwrap()[0].mode, ElicitationMode::Form);
    s.connection.close().await;
}
