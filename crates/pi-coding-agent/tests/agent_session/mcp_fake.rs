//! A docs MCP server on in-memory transports (upstream `createFakeServer` in
//! `agent-session-mcp.test.ts`): tools `search`, `fail`, and `shot`, optional resources, and a
//! transport factory that records the servers it starts and the calls they receive.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use pi_coding_agent::agent_session::McpSessionOptions;
use pi_mcp::runtime::{InMemoryAuthStorageBackend, McpOAuthCredentialStore, TransportFactory};
use pi_mcp::{
    InMemoryTransport, JsonRpcMessage, LATEST_PROTOCOL_VERSION, LoadedMcpConfig, McpServerEntry, McpTransport,
    SharedTransport, TransportEvent, in_memory_pair,
};
use serde_json::{Value, json};

pub const TINY_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";

pub fn server_tools() -> Value {
    json!([
        {
            "name": "search",
            "description": "Search the docs.",
            "inputSchema": { "type": "object", "properties": { "query": { "type": "string" } }, "required": ["query"] },
            "outputSchema": { "type": "object", "properties": { "hits": { "type": "array", "items": { "type": "string" } } }, "required": ["hits"] }
        },
        {
            "name": "fail",
            "description": "Always fails.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "title": "Fail", "destructiveHint": true, "readOnlyHint": false, "idempotentHint": "yes" }
        },
        { "name": "shot", "description": "Returns an image.", "inputSchema": { "type": "object", "properties": {} } }
    ])
}

#[derive(Clone, Default)]
pub struct FakeOptions {
    pub resources: bool,
    pub instructions: Option<String>,
    /// Delay of the answer to `initialize`; `Duration::MAX` never answers.
    pub initialize_delay: Option<Duration>,
}

#[derive(Clone)]
pub struct Fake {
    pub calls: Arc<Mutex<Vec<String>>>,
    pub tools: Arc<Mutex<Value>>,
    /// Server ends of the transports, in connection order.
    pub servers: Arc<Mutex<Vec<Arc<InMemoryTransport>>>>,
    pub connected: Arc<Mutex<Vec<String>>>,
    pub options: FakeOptions,
}

impl Fake {
    pub fn new(options: FakeOptions) -> Self {
        Self {
            calls: Arc::default(),
            tools: Arc::new(Mutex::new(server_tools())),
            servers: Arc::default(),
            connected: Arc::default(),
            options,
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// Tell the clients that the tool list changed.
    pub async fn list_changed(&self) {
        let servers = self.servers.lock().unwrap().clone();
        for server in servers {
            let _ = server.send(&JsonRpcMessage::notification("notifications/tools/list_changed", None)).await;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    pub fn factory(&self) -> TransportFactory {
        let fake = self.clone();
        Arc::new(move |entry, _, _| {
            fake.connected.lock().unwrap().push(entry.name.clone());
            let (client, server) = in_memory_pair();
            let mut events = server.start_channel().now_or_never().expect("starts at once").expect("starts");
            fake.servers.lock().unwrap().push(server.clone());
            let fake = fake.clone();
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    let TransportEvent::Message(JsonRpcMessage::Request { id, method, params }) = event else {
                        continue;
                    };
                    let (fake, server) = (fake.clone(), server.clone());
                    tokio::spawn(async move {
                        if let Some(result) = fake.respond(&method, params.unwrap_or_default()).await {
                            let _ = server.send(&JsonRpcMessage::result(id, result)).await;
                        }
                    });
                }
            });
            Ok(client as SharedTransport)
        })
    }

    async fn respond(&self, method: &str, params: Value) -> Option<Value> {
        Some(match method {
            "initialize" => {
                match self.options.initialize_delay {
                    Some(Duration::MAX) => return None,
                    Some(delay) => tokio::time::sleep(delay).await,
                    None => {}
                }
                let mut capabilities = json!({ "tools": { "listChanged": true } });
                if self.options.resources {
                    capabilities["resources"] = json!({});
                }
                let mut result = json!({ "protocolVersion": LATEST_PROTOCOL_VERSION, "capabilities": capabilities,
                    "serverInfo": { "name": "docs", "version": "1.0.0" } });
                if let Some(instructions) = &self.options.instructions {
                    result["instructions"] = json!(instructions);
                }
                result
            }
            "tools/list" => json!({ "tools": self.tools.lock().unwrap().clone() }),
            "resources/list" => json!({ "resources": [
                { "uri": "docs://intro", "name": "intro", "mimeType": "text/markdown" },
                { "uri": "ui://docs/viewer", "name": "viewer", "mimeType": "text/html;profile=mcp-app" }
            ] }),
            "resources/templates/list" => {
                json!({ "resourceTemplates": [{ "uriTemplate": "docs://pages/{slug}", "name": "page" }] })
            }
            "resources/read" => {
                let uri = params["uri"].as_str().unwrap_or_default().to_string();
                self.calls.lock().unwrap().push(format!("read:{uri}"));
                json!({ "contents": [{ "uri": uri, "mimeType": "text/markdown", "text": format!("# {uri}") }] })
            }
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default().to_string();
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                self.calls.lock().unwrap().push(format!("{name}:{arguments}"));
                match name.as_str() {
                    "search" => {
                        let query = arguments["query"].as_str().unwrap_or_default();
                        let hits = [format!("{query} guide"), format!("{query} faq")];
                        json!({ "content": [{ "type": "text", "text": hits.join("\n") }], "structuredContent": { "hits": hits } })
                    }
                    "shot" => json!({ "content": [{ "type": "image", "data": TINY_PNG, "mimeType": "image/png" }] }),
                    _ => json!({ "content": [{ "type": "text", "text": "server exploded" }], "isError": true }),
                }
            }
            _ => json!({}),
        })
    }

    /// Session MCP options with one server `name` of `config` served by this fake.
    pub fn session_options(&self, name: &str, config: Value, extra: Value) -> McpSessionOptions {
        let mut entry: McpServerEntry =
            McpServerEntry::new(name, serde_json::from_value(config).expect("server config"), "test");
        entry.scope = None;
        let auto_enable = extra.get("autoEnableCodemode").and_then(Value::as_bool);
        let loaded = LoadedMcpConfig { servers: vec![entry], auto_enable_codemode: auto_enable, ..Default::default() };
        McpSessionOptions {
            load_config: Some(Arc::new(move |_| loaded.clone())),
            create_transport: Some(self.factory()),
            credentials: Some(McpOAuthCredentialStore::with_backend(
                Arc::new(InMemoryAuthStorageBackend::default()),
                None,
            )),
            startup_wait: extra.get("startupWaitMs").and_then(Value::as_u64).map(Duration::from_millis),
            update_config: Some(Arc::new(|_, _| Ok(()))),
            ..McpSessionOptions::default()
        }
    }
}
