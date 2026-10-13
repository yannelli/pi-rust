//! Test helpers: an in-memory MCP server and a minimal HTTP/1.1 server.
#![allow(dead_code)]

pub mod fake_mcp;
pub mod http;
pub mod oauth_server;
pub mod provider;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use pi_mcp::{
    InMemoryTransport, JsonRpcErrorObject, JsonRpcMessage, LATEST_PROTOCOL_VERSION, McpClient, McpClientOptions,
    McpError, McpTransport, TransportEvent, in_memory_pair,
};
use serde_json::{Value, json};

pub type Handler = Arc<dyn Fn(Option<Value>) -> BoxFuture<'static, Result<Value, McpError>> + Send + Sync>;

/// A scripted server on the far end of an in-memory transport.
#[derive(Clone)]
pub struct TestServer {
    pub transport: Arc<InMemoryTransport>,
    pub messages: Arc<Mutex<Vec<JsonRpcMessage>>>,
    handlers: Arc<Mutex<HashMap<String, Handler>>>,
}

impl TestServer {
    pub fn set_handler<F, Fut>(&self, method: &str, handler: F)
    where
        F: Fn(Option<Value>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<Value, McpError>> + Send + 'static,
    {
        let handler: Handler = Arc::new(move |params| Box::pin(handler(params)));
        self.handlers.lock().unwrap().insert(method.into(), handler);
    }

    pub fn set_result(&self, method: &str, result: Value) {
        self.set_handler(method, move |_| {
            let result = result.clone();
            async move { Ok(result) }
        });
    }

    pub fn messages(&self) -> Vec<JsonRpcMessage> {
        self.messages.lock().unwrap().clone()
    }

    pub fn methods(&self) -> Vec<String> {
        self.messages().iter().filter_map(|message| message.method().map(str::to_string)).collect()
    }

    pub async fn send(&self, message: JsonRpcMessage) {
        self.transport.send(&message).await.expect("server send");
    }
}

/// A connected pair: the client transport and the server answering on the other end.
pub async fn create_server() -> (Arc<InMemoryTransport>, TestServer) {
    let (client, server) = in_memory_pair();
    let mut events = server.start_channel().await.unwrap();
    let test_server = TestServer { transport: server, messages: Arc::default(), handlers: Arc::default() };
    test_server.set_result(
        "initialize",
        json!({
            "protocolVersion": LATEST_PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": true } },
            "serverInfo": { "name": "test-server", "version": "1.0.0" },
            "instructions": "Use test tools."
        }),
    );
    let server = test_server.clone();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let TransportEvent::Message(message) = event else { continue };
            server.messages.lock().unwrap().push(message.clone());
            let JsonRpcMessage::Request { id, method, params } = message else { continue };
            let handler = server.handlers.lock().unwrap().get(&method).cloned();
            let transport = server.transport.clone();
            tokio::spawn(async move {
                let result = match handler {
                    Some(handler) => handler(params).await,
                    None => Err(McpError::rpc(-32601, format!("Method not found: {method}"))),
                };
                let response = match result {
                    Ok(result) => JsonRpcMessage::result(id, result),
                    Err(McpError::Rpc { code, message, data }) => {
                        JsonRpcMessage::Response { id, result: Err(JsonRpcErrorObject { code, message, data }) }
                    }
                    Err(error) => JsonRpcMessage::error(id, -32603, error.to_string()),
                };
                let _ = transport.send(&response).await;
            });
        }
    });
    (client, test_server)
}

pub async fn connect() -> (McpClient, TestServer) {
    let (transport, server) = create_server().await;
    let client = McpClient::new(McpClientOptions::new("test-client", "2.0.0"));
    client.connect(transport).await.unwrap();
    (client, server)
}

pub async fn settle() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

/// Poll until `check` holds, for up to five seconds.
pub async fn wait_until(mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not met in time");
}
