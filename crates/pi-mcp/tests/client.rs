//! Client protocol tests against an in-memory server, ported from `packages/mcp/test/client.test.ts`.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{connect, create_server, settle};
use pi_mcp::{
    ClientState, JsonRpcMessage, LATEST_PROTOCOL_VERSION, McpClient, McpClientOptions, McpError, RequestOptions, Root,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn value<T: serde::Serialize>(item: T) -> Value {
    serde_json::to_value(item).unwrap()
}

#[tokio::test]
async fn initializes_the_connection_before_exposing_server_information() {
    let (client, server) = connect().await;
    assert_eq!(client.connection_state(), ClientState::Connected);
    assert_eq!(client.protocol_version().as_deref(), Some(LATEST_PROTOCOL_VERSION));
    assert_eq!(value(client.server_info().unwrap()), json!({ "name": "test-server", "version": "1.0.0" }));
    assert_eq!(value(client.server_capabilities().unwrap()), json!({ "tools": { "listChanged": true } }));
    assert_eq!(client.instructions().as_deref(), Some("Use test tools."));
    settle().await;
    let messages: Vec<Value> = server.messages().iter().map(JsonRpcMessage::to_value).collect();
    assert_eq!(
        messages,
        vec![
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": LATEST_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "test-client", "version": "2.0.0" }
                }
            }),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        ]
    );
    client.close().await;
}

#[tokio::test]
async fn paginates_tools_and_preserves_protocol_tool_definitions() {
    let (client, server) = connect().await;
    server.set_handler("tools/list", |params| async move {
        Ok(match params.and_then(|params| params.get("cursor").cloned()) {
            None => json!({ "tools": [{ "name": "search", "description": "Search", "inputSchema": { "type": "object" } }], "nextCursor": "page-2" }),
            // Some servers end pagination with an empty cursor instead of omitting it.
            Some(_) => json!({
                "tools": [{ "name": "read", "inputSchema": { "type": "object" }, "outputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } }],
                "nextCursor": ""
            }),
        })
    });
    let tools = client.list_tools(RequestOptions::default()).await.unwrap();
    assert_eq!(
        value(tools),
        json!([
            { "name": "search", "description": "Search", "inputSchema": { "type": "object" } },
            { "name": "read", "inputSchema": { "type": "object" }, "outputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } }
        ])
    );
    server.set_result("tools/list", json!({ "tools": [], "nextCursor": "same" }));
    let error = client.list_tools(RequestOptions::default()).await.unwrap_err();
    assert_eq!(error.to_string(), "MCP tools/list returned duplicate cursor: same");
    client.close().await;
}

#[tokio::test]
async fn lists_and_reads_resources() {
    let (client, server) = connect().await;
    server.set_handler("resources/list", |params| async move {
        Ok(match params.and_then(|params| params.get("cursor").cloned()) {
            None => json!({ "resources": [{ "uri": "file:///a", "name": "a", "mimeType": "text/plain" }], "nextCursor": "2" }),
            Some(_) => json!({ "resources": [{ "uri": "file:///b" }] }),
        })
    });
    server.set_result(
        "resources/templates/list",
        json!({ "resourceTemplates": [{ "uriTemplate": "repo://{owner}/{repo}", "name": "repo" }] }),
    );
    server.set_handler("resources/read", |params| async move {
        Ok(json!({ "contents": [{ "uri": params.unwrap()["uri"], "text": "hello" }] }))
    });
    let options = RequestOptions::default;
    // A missing name falls back to the URI.
    assert_eq!(
        value(client.list_resources(options()).await.unwrap()),
        json!([{ "uri": "file:///a", "name": "a", "mimeType": "text/plain" }, { "uri": "file:///b", "name": "file:///b" }])
    );
    assert_eq!(
        value(client.list_resource_templates(options()).await.unwrap()),
        json!([{ "uriTemplate": "repo://{owner}/{repo}", "name": "repo" }])
    );
    assert_eq!(
        value(client.list_resources_page(None, options()).await.unwrap()),
        json!({ "resources": [{ "uri": "file:///a", "name": "a", "mimeType": "text/plain" }], "nextCursor": "2" })
    );
    assert_eq!(
        value(client.list_resources_page(Some("2"), options()).await.unwrap()),
        json!({ "resources": [{ "uri": "file:///b", "name": "file:///b" }] })
    );
    assert_eq!(
        value(client.read_resource("file:///a", options()).await.unwrap()),
        json!({ "contents": [{ "uri": "file:///a", "text": "hello" }] })
    );

    server.set_result("resources/read", json!({ "contents": [{ "uri": "file:///a" }] }));
    let error = client.read_resource("file:///a", options()).await.unwrap_err();
    assert_eq!(error.to_string(), "Invalid contents in MCP resources/read result");
    server.set_result("resources/list", json!({ "resources": [{ "name": "no uri" }] }));
    let error = client.list_resources(options()).await.unwrap_err();
    assert_eq!(error.to_string(), "Invalid entry in MCP resources/list result");
    client.close().await;
}

#[tokio::test]
async fn returns_structured_tool_content_and_surfaces_json_rpc_errors() {
    let (client, server) = connect().await;
    server.set_handler("tools/call", |params| async move {
        let params = params.unwrap();
        if params["name"] == "fail" {
            return Err(McpError::Rpc { code: 1234, message: "tool failed".into(), data: Some(json!({ "retryable": false })) });
        }
        Ok(json!({ "content": [{ "type": "text", "text": "ok" }], "structuredContent": { "count": params["arguments"]["count"] } }))
    });
    let result = client.call_tool("count", Some(json!({ "count": 3 })), RequestOptions::default()).await.unwrap();
    assert_eq!(
        value(result),
        json!({ "content": [{ "type": "text", "text": "ok" }], "structuredContent": { "count": 3 } })
    );
    match client.call_tool("fail", None, RequestOptions::default()).await.unwrap_err() {
        McpError::Rpc { code, message, data } => {
            assert_eq!((code, message.as_str(), data), (1234, "tool failed", Some(json!({ "retryable": false }))));
        }
        other => panic!("unexpected error {other:?}"),
    }
    client.close().await;
}

#[tokio::test(start_paused = true)]
async fn renews_the_timeout_on_progress() {
    let (client, server) = connect().await;
    let transport = server.transport.clone();
    server.set_handler("tools/call", move |params| {
        let transport = transport.clone();
        async move {
            let token = params.unwrap()["_meta"]["progressToken"].clone();
            tokio::time::sleep(Duration::from_millis(40)).await;
            let progress = json!({ "progressToken": token, "progress": 1, "total": 2 });
            pi_mcp::McpTransport::send(
                transport.as_ref(),
                &JsonRpcMessage::notification("notifications/progress", Some(progress)),
            )
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(40)).await;
            Ok(json!({ "content": [{ "type": "text", "text": "done" }] }))
        }
    });
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let options = RequestOptions {
        timeout: Some(Duration::from_millis(50)),
        on_progress: Some(Arc::new(move |progress| recorder.lock().unwrap().push(value(progress)))),
        ..RequestOptions::default()
    };
    let result = client.call_tool("slow", Some(json!({})), options).await.unwrap();
    assert_eq!(value(result), json!({ "content": [{ "type": "text", "text": "done" }] }));
    assert_eq!(*seen.lock().unwrap(), vec![json!({ "progressToken": 2, "progress": 1.0, "total": 2.0 })]);

    // Without progress, the same call times out.
    let options = RequestOptions { timeout: Some(Duration::from_millis(30)), ..RequestOptions::default() };
    let error = client.call_tool("slow", Some(json!({})), options).await.unwrap_err();
    assert!(matches!(error, McpError::Timeout(30)), "{error:?}");
    client.close().await;
}

#[tokio::test]
async fn cancels_aborted_and_timed_out_requests() {
    let (client, server) = connect().await;
    server.set_handler("tools/call", |_| futures::future::pending());
    let cancel = CancellationToken::new();
    let options = RequestOptions { cancel: Some(cancel.clone()), ..RequestOptions::default() };
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.call_tool("wait", Some(json!({})), options).await }
    });
    settle().await;
    cancel.cancel();
    assert!(matches!(call.await.unwrap(), Err(McpError::Aborted)));
    settle().await;
    let cancelled =
        JsonRpcMessage::notification("notifications/cancelled", Some(json!({ "requestId": 2, "reason": "Aborted" })));
    assert!(server.messages().contains(&cancelled));

    let options = RequestOptions { timeout: Some(Duration::from_millis(5)), ..RequestOptions::default() };
    let error = client.call_tool("wait", Some(json!({})), options).await.unwrap_err();
    assert!(matches!(error, McpError::Timeout(5)));
    settle().await;
    let reason = json!({ "requestId": 3, "reason": "Request timed out" });
    assert!(server.messages().contains(&JsonRpcMessage::notification("notifications/cancelled", Some(reason))));
    client.close().await;
}

#[tokio::test]
async fn reports_transport_errors_without_failing_pending_requests() {
    let (transport, server) = create_server().await;
    let client = McpClient::new(McpClientOptions::new("test-client", "1.0.0"));
    client.connect(transport.clone()).await.unwrap();
    let errors = Arc::new(Mutex::new(Vec::new()));
    let sink = errors.clone();
    client.on_error(move |error| sink.lock().unwrap().push(error.to_string()));
    let (release, gate) = tokio::sync::oneshot::channel::<()>();
    let gate = Arc::new(tokio::sync::Mutex::new(Some(gate)));
    server.set_handler("tools/call", move |_| {
        let gate = gate.clone();
        async move {
            if let Some(gate) = gate.lock().await.take() {
                let _ = gate.await;
            }
            Ok(json!({ "content": [] }))
        }
    });
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.call_tool("wait", None, RequestOptions::default()).await }
    });
    settle().await;
    transport.emit_error(McpError::other("stray log line"));
    release.send(()).unwrap();
    assert_eq!(value(call.await.unwrap().unwrap()), json!({ "content": [] }));
    assert_eq!(*errors.lock().unwrap(), vec!["stray log line".to_string()]);
    client.close().await;
}

#[tokio::test]
async fn accepts_servers_that_answer_with_an_older_protocol_version() {
    let (transport, server) = create_server().await;
    server.set_result(
        "initialize",
        json!({ "protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": { "name": "old-server", "version": "0.1.0" } }),
    );
    let client = McpClient::new(McpClientOptions::new("test-client", "1.0.0"));
    client.connect(transport).await.unwrap();
    assert_eq!(client.protocol_version().as_deref(), Some("2024-11-05"));
    client.close().await;

    let (transport, server) = create_server().await;
    server.set_result(
        "initialize",
        json!({ "protocolVersion": "1999-01-01", "capabilities": {}, "serverInfo": { "name": "ancient", "version": "0.1.0" } }),
    );
    let rejected = McpClient::new(McpClientOptions::new("test-client", "1.0.0"));
    let error = rejected.connect(transport).await.unwrap_err();
    assert!(error.to_string().contains("unsupported protocol version"));
    assert_eq!(rejected.connection_state(), ClientState::Closed);
}

#[tokio::test]
async fn defaults_missing_tool_result_content_to_an_empty_list() {
    let (client, server) = connect().await;
    server.set_result("tools/call", json!({ "structuredContent": { "ok": true } }));
    let result = client.call_tool("structured", None, RequestOptions::default()).await.unwrap();
    assert_eq!(value(result), json!({ "content": [], "structuredContent": { "ok": true } }));
    server.set_result("tools/call", json!({ "content": "not a list" }));
    let error = client.call_tool("broken", None, RequestOptions::default()).await.unwrap_err();
    assert_eq!(error.to_string(), "Invalid MCP tools/call result");
    client.close().await;
}

// #10416: MCP 2026-07-28 allows any JSON value as structured content.
#[tokio::test]
async fn accepts_structured_content_that_is_not_an_object() {
    let (client, server) = connect().await;
    for structured in [json!([{ "id": 1 }, { "id": 2 }]), Value::Null, json!("text"), json!(3), json!(false)] {
        server.set_result("tools/call", json!({ "content": [], "structuredContent": structured }));
        let result = client.call_tool("structured", None, RequestOptions::default()).await.unwrap();
        assert_eq!(value(result), json!({ "content": [], "structuredContent": structured }));
    }
    client.close().await;
}

#[tokio::test]
async fn does_not_send_notifications_cancelled_for_a_timed_out_initialize() {
    let (transport, server) = create_server().await;
    server.set_handler("initialize", |_| futures::future::pending());
    let options =
        McpClientOptions { request_timeout: Some(Duration::from_millis(5)), ..McpClientOptions::new("c", "1") };
    let client = McpClient::new(options);
    assert!(matches!(client.connect(transport).await, Err(McpError::Timeout(5))));
    settle().await;
    assert!(!server.methods().iter().any(|method| method == "notifications/cancelled"));
}

#[tokio::test]
async fn notifies_close_listeners_once_when_the_transport_drops() {
    let (client, server) = connect().await;
    let closed = Arc::new(AtomicUsize::new(0));
    let counter = closed.clone();
    client.on_close(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    server.set_handler("tools/call", |_| futures::future::pending());
    let pending = tokio::spawn({
        let client = client.clone();
        async move { client.call_tool("wait", None, RequestOptions::default()).await }
    });
    settle().await;
    pi_mcp::McpTransport::close(server.transport.as_ref()).await;
    let error = pending.await.unwrap().unwrap_err();
    assert_eq!(error.to_string(), "MCP connection closed");
    assert_eq!(client.connection_state(), ClientState::Closed);
    client.close().await;
    assert_eq!(closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn answers_roots_list_and_dispatches_notifications() {
    let (transport, server) = create_server().await;
    let roots = vec![Root { uri: "file:///workspace".into(), name: Some("workspace".into()) }];
    let client =
        McpClient::new(McpClientOptions { roots: Some(roots), ..McpClientOptions::new("test-client", "1.0.0") });
    client.connect(transport).await.unwrap();
    let changed = Arc::new(Mutex::new(Vec::new()));
    let sink = changed.clone();
    client
        .on_notification("notifications/tools/list_changed", move |params| sink.lock().unwrap().push(params.cloned()));
    server.send(JsonRpcMessage::request("roots", "roots/list", None)).await;
    server.send(JsonRpcMessage::request("p", "ping", None)).await;
    server.send(JsonRpcMessage::request("x", "sampling/createMessage", None)).await;
    server.send(JsonRpcMessage::notification("notifications/tools/list_changed", None)).await;
    settle().await;
    let messages = server.messages();
    let roots = json!({ "roots": [{ "uri": "file:///workspace", "name": "workspace" }] });
    assert!(messages.contains(&JsonRpcMessage::result("roots".into(), roots)));
    assert!(messages.contains(&JsonRpcMessage::result("p".into(), json!({}))));
    assert!(messages.contains(&JsonRpcMessage::error("x".into(), -32601, "Method not found: sampling/createMessage")));
    assert_eq!(*changed.lock().unwrap(), vec![None]);
    // `initialize` announced the roots capability.
    assert_eq!(server.messages()[0].to_value()["params"]["capabilities"], json!({ "roots": {} }));
    client.close().await;
}
