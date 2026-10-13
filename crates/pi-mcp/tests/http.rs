//! Streamable HTTP transport tests, ported from `packages/mcp/test/streamable-http.test.ts`.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::http::{Request, Response, serve};
use pi_mcp::{
    AuthProvider, LATEST_PROTOCOL_VERSION, McpClient, McpClientOptions, McpError, ReconnectOptions, RequestOptions,
    StreamableHttpOptions, StreamableHttpTransport, UnauthorizedContext,
};
use serde_json::{Value, json};
use url::Url;

type Requests = Arc<Mutex<Vec<Request>>>;

/// Answers like a stateful MCP server: JSON for initialize and tools/list, SSE for other requests.
fn protocol(request: &Request) -> Response {
    match request.method.as_str() {
        "GET" => return Response::status(405),
        "DELETE" => return Response::status(200),
        _ => {}
    }
    let message = request.json();
    if message.get("id").is_none() {
        return Response::status(202);
    }
    let id = message["id"].clone();
    match message["method"].as_str().unwrap_or_default() {
        "initialize" => Response::json(
            200,
            &json!({ "jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": LATEST_PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "http-fixture", "version": "1.0.0" }
            } }),
        )
        .header("mcp-session-id", "session-1"),
        "tools/list" => Response::json(
            200,
            &json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }] } }),
        ),
        _ => {
            let result =
                json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [{ "type": "text", "text": "hello" }] } });
            Response::sse(format!("id: tool-result\ndata: {result}\n\n"))
        }
    }
}

async fn start<F, Fut>(handler: F) -> (Url, Requests)
where
    F: Fn(Request, Requests) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Response> + Send + 'static,
{
    let requests: Requests = Arc::default();
    let recorded = requests.clone();
    let origin = serve(move |request, _| {
        recorded.lock().unwrap().push(request.clone());
        handler(request, recorded.clone())
    })
    .await;
    (Url::parse(&format!("{origin}/mcp")).unwrap(), requests)
}

fn transport(url: &Url, configure: impl FnOnce(&mut StreamableHttpOptions)) -> Arc<StreamableHttpTransport> {
    let mut options = StreamableHttpOptions::new(url.clone());
    configure(&mut options);
    Arc::new(StreamableHttpTransport::new(options).unwrap())
}

fn client() -> McpClient {
    McpClient::new(McpClientOptions::new("http-test", "1.0.0"))
}

fn hello() -> Value {
    json!({ "content": [{ "type": "text", "text": "hello" }] })
}

async fn call(client: &McpClient, name: &str) -> Result<Value, McpError> {
    let options = RequestOptions { timeout: Some(Duration::from_secs(5)), ..RequestOptions::default() };
    client.call_tool(name, None, options).await.map(|result| serde_json::to_value(result).unwrap())
}

#[tokio::test]
async fn handles_json_and_sse_responses_with_session_and_protocol_headers() {
    let (url, requests) = start(|request, _| async move { protocol(&request) }).await;
    let transport = transport(&url, |_| {});
    let client = client();
    client.connect(transport.clone()).await.unwrap();
    assert_eq!(transport.session_id().as_deref(), Some("session-1"));
    assert_eq!(client.list_tools(RequestOptions::default()).await.unwrap()[0].name, "echo");
    assert_eq!(call(&client, "echo").await.unwrap(), hello());
    common::wait_until(|| requests.lock().unwrap().iter().any(|request| request.method == "GET")).await;
    client.close().await;
    let requests = requests.lock().unwrap();
    let list = requests.iter().find(|request| request.json()["method"] == "tools/list").unwrap();
    assert_eq!(list.header("mcp-session-id"), Some("session-1"));
    assert_eq!(list.header("mcp-protocol-version"), Some(LATEST_PROTOCOL_VERSION));
    assert!(requests.iter().any(|request| request.method == "DELETE"));
    // The GET stream opens only after initialization and never resumes without an event ID.
    let get = requests.iter().position(|request| request.method == "GET").unwrap();
    let initialized =
        requests.iter().position(|request| request.json()["method"] == "notifications/initialized").unwrap();
    assert!(get > initialized);
    assert!(requests.iter().all(|request| request.header("last-event-id").is_none()));
}

#[tokio::test]
async fn classifies_authentication_failures() {
    let (url, _) = start(|_, _| async move {
        Response::text(401, "login required")
            .header("www-authenticate", r#"Bearer resource_metadata="https://example.com/meta""#)
    })
    .await;
    match client().connect(transport(&url, |_| {})).await.unwrap_err() {
        McpError::AuthRequired { body, www_authenticate } => {
            assert_eq!(body, "login required");
            assert_eq!(www_authenticate.as_deref(), Some(r#"Bearer resource_metadata="https://example.com/meta""#));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn fails_only_the_request_whose_sse_stream_breaks() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let release = gate.clone();
    let (url, _) = start(move |request, _| {
        let gate = gate.clone();
        async move {
            let name = request.json()["params"]["name"].as_str().unwrap_or_default().to_string();
            if name == "broken" {
                return Response::sse("data: not json\n\n");
            }
            if name == "slow" {
                gate.notified().await;
            }
            protocol(&request)
        }
    })
    .await;
    let client = client();
    let errors = Arc::new(AtomicUsize::new(0));
    let counter = errors.clone();
    client.on_error(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    client.connect(transport(&url, |options| options.open_get_stream = false)).await.unwrap();
    let slow = tokio::spawn({
        let client = client.clone();
        async move { call(&client, "slow").await }
    });
    let error = call(&client, "broken").await.unwrap_err();
    assert!(error.to_string().starts_with("MCP response stream failed"), "{error}");
    release.notify_one();
    assert_eq!(slow.await.unwrap().unwrap(), hello());
    assert_eq!(errors.load(Ordering::SeqCst), 1);
    client.close().await;
}

#[tokio::test]
async fn resumes_a_response_stream_the_server_closed_before_answering() {
    let (url, requests) = start(|request, _| async move {
        if request.method == "GET" && request.header("last-event-id").is_some() {
            let result =
                json!({ "jsonrpc": "2.0", "id": 2, "result": { "content": [{ "type": "text", "text": "resumed" }] } });
            return Response::sse(format!("id: 2\ndata: {result}\n\n"));
        }
        if request.method == "POST" && request.json()["method"] == "tools/call" {
            // Priming event (ID, no data) and a retry hint, then the server drops the stream.
            return Response::sse("id: 1\nretry: 5\ndata:\n\n");
        }
        protocol(&request)
    })
    .await;
    let client = client();
    let errors = Arc::new(AtomicUsize::new(0));
    let counter = errors.clone();
    client.on_error(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    client.connect(transport(&url, |options| options.open_get_stream = false)).await.unwrap();
    assert_eq!(call(&client, "echo").await.unwrap(), json!({ "content": [{ "type": "text", "text": "resumed" }] }));
    let resumed: Vec<String> = requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| request.header("last-event-id").map(str::to_string))
        .collect();
    assert_eq!(resumed, vec!["1".to_string()]);
    assert_eq!(errors.load(Ordering::SeqCst), 0);
    client.close().await;
}

#[tokio::test]
async fn fails_a_request_whose_response_stream_ends_without_an_answer() {
    let (url, _) = start(|request, _| async move {
        if request.method == "POST" && request.json()["method"] == "tools/call" {
            return Response::sse(": nothing here\n\n");
        }
        protocol(&request)
    })
    .await;
    let client = client();
    client.connect(transport(&url, |options| options.open_get_stream = false)).await.unwrap();
    let error = call(&client, "echo").await.unwrap_err();
    assert_eq!(error.to_string(), "MCP response stream failed: stream ended without a response");
    client.close().await;
}

#[tokio::test]
async fn reconnects_the_get_stream_after_it_drops_and_retries_transient_statuses() {
    let gets = Arc::new(AtomicUsize::new(0));
    let open_streams = Arc::new(Mutex::new(Vec::new()));
    let (url, requests) = start({
        let gets = gets.clone();
        move |request, _| {
            let gets = gets.clone();
            let open_streams = open_streams.clone();
            async move {
                if request.method != "GET" {
                    return protocol(&request);
                }
                let notification = json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" });
                match gets.fetch_add(1, Ordering::SeqCst) {
                    0 => Response::sse(format!("id: g1\ndata: {notification}\n\n")),
                    // Overloaded: worth another try.
                    1 => Response::text(503, "busy"),
                    _ => {
                        let (sender, chunks) = tokio::sync::mpsc::unbounded_channel();
                        sender.send(format!("id: g2\ndata: {notification}\n\n")).unwrap();
                        open_streams.lock().unwrap().push(sender);
                        Response::Stream {
                            status: 200,
                            headers: vec![("content-type".into(), "text/event-stream".into())],
                            chunks,
                        }
                    }
                }
            }
        }
    })
    .await;
    let client = client();
    let changes = Arc::new(AtomicUsize::new(0));
    let counter = changes.clone();
    client.on_notification("notifications/tools/list_changed", move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    let reconnect = ReconnectOptions { initial_delay: Duration::from_millis(1), ..ReconnectOptions::default() };
    client.connect(transport(&url, |options| options.reconnect = reconnect)).await.unwrap();
    common::wait_until(|| changes.load(Ordering::SeqCst) == 2).await;
    let last_event_ids: Vec<Option<String>> = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.method == "GET")
        .map(|request| request.header("last-event-id").map(str::to_string))
        .collect();
    assert_eq!(last_event_ids, vec![None, Some("g1".into()), Some("g1".into())]);
    client.close().await;
}

#[tokio::test]
async fn rejects_a_request_the_server_accepts_without_a_response() {
    let (url, _) = start(|request, _| async move {
        if request.method == "POST" && request.json()["method"] == "tools/call" {
            return Response::status(202);
        }
        protocol(&request)
    })
    .await;
    let client = client();
    client.connect(transport(&url, |options| options.open_get_stream = false)).await.unwrap();
    let error = call(&client, "echo").await.unwrap_err();
    assert!(error.to_string().contains("without a response"), "{error}");
    client.close().await;
}

#[tokio::test]
async fn includes_the_response_body_in_http_errors() {
    let (url, _) = start(|_, _| async move { Response::text(400, "Invalid Accept header") }).await;
    let error = client().connect(transport(&url, |_| {})).await.unwrap_err();
    assert_eq!(error.to_string(), "MCP HTTP request failed with status 400: Invalid Accept header");
}

struct ScriptedAuth {
    token: Mutex<String>,
    seen: Mutex<Vec<(u16, Option<String>)>>,
    calls: AtomicUsize,
}

#[async_trait]
impl AuthProvider for ScriptedAuth {
    async fn token(&self) -> Result<Option<String>, McpError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(self.token.lock().unwrap().clone()))
    }

    async fn on_unauthorized(&self, context: UnauthorizedContext) -> Result<bool, McpError> {
        self.seen.lock().unwrap().push((context.status, context.token.clone()));
        *self.token.lock().unwrap() = if context.status == 401 { "new".into() } else { "admin".into() };
        Ok(true)
    }
}

#[tokio::test]
async fn hands_401_and_insufficient_scope_403_responses_to_the_auth_provider_with_the_rejected_token() {
    let (url, _) = start(|request, _| async move {
        if request.method != "POST" {
            return protocol(&request);
        }
        if request.header("authorization") == Some("Bearer old") {
            return Response::status(401).header("www-authenticate", "Bearer");
        }
        if request.json()["method"] == "tools/call" && request.header("authorization") == Some("Bearer new") {
            return Response::status(403)
                .header("www-authenticate", r#"Bearer error="insufficient_scope", scope="admin""#);
        }
        protocol(&request)
    })
    .await;
    let auth =
        Arc::new(ScriptedAuth { token: Mutex::new("old".into()), seen: Mutex::default(), calls: AtomicUsize::new(0) });
    let client = client();
    let provider = auth.clone();
    client
        .connect(transport(&url, move |options| {
            options.open_get_stream = false;
            options.auth_provider = Some(provider);
        }))
        .await
        .unwrap();
    assert_eq!(call(&client, "echo").await.unwrap(), hello());
    assert_eq!(*auth.seen.lock().unwrap(), vec![(401, Some("old".into())), (403, Some("new".into()))]);
    client.close().await;
}

#[tokio::test]
async fn closes_the_session_with_the_last_requests_token_without_asking_the_auth_provider() {
    let (url, requests) = start(|request, _| async move { protocol(&request) }).await;
    let auth = Arc::new(ScriptedAuth {
        token: Mutex::new("token".into()),
        seen: Mutex::default(),
        calls: AtomicUsize::new(0),
    });
    let client = client();
    let provider = auth.clone();
    client
        .connect(transport(&url, move |options| {
            options.open_get_stream = false;
            options.auth_provider = Some(provider);
        }))
        .await
        .unwrap();
    let before = auth.calls.load(Ordering::SeqCst);
    client.close().await;
    assert_eq!(auth.calls.load(Ordering::SeqCst), before);
    let requests = requests.lock().unwrap();
    let deletes: Vec<&Request> = requests.iter().filter(|request| request.method == "DELETE").collect();
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0].header("authorization"), Some("Bearer token"));
    assert_eq!(deletes[0].header("mcp-session-id"), Some("session-1"));
}

#[tokio::test]
async fn classifies_an_expired_established_session() {
    let posts = Arc::new(AtomicUsize::new(0));
    let (url, _) = start(move |request, _| {
        let posts = posts.clone();
        async move {
            if request.method == "POST" && posts.fetch_add(1, Ordering::SeqCst) >= 2 {
                return Response::text(404, "gone");
            }
            protocol(&request)
        }
    })
    .await;
    let client = client();
    client.connect(transport(&url, |options| options.open_get_stream = false)).await.unwrap();
    let error = client.list_tools(RequestOptions::default()).await.unwrap_err();
    assert!(matches!(error, McpError::SessionExpired { .. }), "{error:?}");
    client.close().await;
}
