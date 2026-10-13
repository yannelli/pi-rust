//! A docs server for manager tests: tools `search`, `fail`, and `shot`, optional resources, and a
//! manager wired to it. Ported from the fake server of coding-agent's agent-session-mcp.test.ts.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use pi_mcp::runtime::{InMemoryAuthStorageBackend, McpOAuthCredentialStore, TransportFactory};
use pi_mcp::{
    LATEST_PROTOCOL_VERSION, LoadedMcpConfig, McpError, McpManager, McpManagerOptions, McpServerEntry, NotifyLevel,
    SharedTransport,
};
use serde_json::{Value, json};

use super::{TestServer, create_server};

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
    /// Delay of the answer to `initialize`; `None` answers at once, `Some(Duration::MAX)` never.
    pub initialize_delay: Option<Duration>,
}

/// Every server the factory created, the tools they list, and the calls they received.
#[derive(Clone)]
pub struct Fake {
    pub calls: Arc<Mutex<Vec<String>>>,
    pub tools: Arc<Mutex<Value>>,
    pub servers: Arc<Mutex<Vec<TestServer>>>,
    pub options: FakeOptions,
}

impl Fake {
    pub fn new(options: FakeOptions) -> Self {
        Self { calls: Arc::default(), tools: Arc::new(Mutex::new(server_tools())), servers: Arc::default(), options }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn server(&self, index: usize) -> TestServer {
        self.servers.lock().unwrap()[index].clone()
    }

    fn configure(&self, server: &TestServer) {
        let options = self.options.clone();
        let mut capabilities = json!({ "tools": {} });
        if options.resources {
            capabilities["resources"] = json!({});
        }
        let mut initialize = json!({ "protocolVersion": LATEST_PROTOCOL_VERSION, "capabilities": capabilities, "serverInfo": { "name": "docs", "version": "1.0.0" } });
        if let Some(instructions) = &options.instructions {
            initialize["instructions"] = json!(instructions);
        }
        server.set_handler("initialize", move |_| {
            let (initialize, delay) = (initialize.clone(), options.initialize_delay);
            async move {
                match delay {
                    Some(Duration::MAX) => futures::future::pending::<()>().await,
                    Some(delay) => tokio::time::sleep(delay).await,
                    None => {}
                }
                Ok::<_, McpError>(initialize)
            }
        });
        let tools = self.tools.clone();
        server.set_handler("tools/list", move |_| {
            let tools = tools.lock().unwrap().clone();
            async move { Ok(json!({ "tools": tools })) }
        });
        server.set_result(
            "resources/list",
            json!({ "resources": [
                { "uri": "docs://intro", "name": "intro", "mimeType": "text/markdown", "_meta": { "x": 1 } },
                { "uri": "ui://docs/viewer", "name": "viewer", "mimeType": "text/html;profile=mcp-app" }
            ] }),
        );
        server.set_result(
            "resources/templates/list",
            json!({ "resourceTemplates": [{ "uriTemplate": "docs://pages/{slug}", "name": "page", "icons": [{ "src": "data:image/png;base64,AAAA" }] }] }),
        );
        let calls = self.calls.clone();
        server.set_handler("resources/read", move |params| {
            let uri = params.unwrap_or_default()["uri"].as_str().unwrap_or_default().to_string();
            calls.lock().unwrap().push(format!("read:{uri}"));
            async move {
                Ok(json!({ "contents": [{ "uri": uri, "mimeType": "text/markdown", "text": format!("# {uri}") }] }))
            }
        });
        let calls = self.calls.clone();
        server.set_handler("tools/call", move |params| {
            let params = params.unwrap_or_default();
            let (name, arguments) = (params["name"].as_str().unwrap_or_default().to_string(), params["arguments"].clone());
            calls.lock().unwrap().push(format!("{name}:{arguments}"));
            async move {
                Ok(match name.as_str() {
                    "search" => {
                        let query = arguments["query"].as_str().unwrap_or_default();
                        let hits = [format!("{query} guide"), format!("{query} faq")];
                        json!({ "content": [{ "type": "text", "text": hits.join("\n") }], "structuredContent": { "hits": hits } })
                    }
                    "shot" => json!({ "content": [{ "type": "image", "data": TINY_PNG, "mimeType": "image/png" }] }),
                    _ => json!({ "content": [{ "type": "text", "text": "server exploded" }], "isError": true }),
                })
            }
        });
    }

    pub fn factory(&self) -> TransportFactory {
        let fake = self.clone();
        Arc::new(move |_, _, _| {
            let (transport, server) = create_server().now_or_never().expect("in-memory server starts at once");
            fake.configure(&server);
            fake.servers.lock().unwrap().push(server);
            Ok(transport as SharedTransport)
        })
    }
}

pub struct Setup {
    pub manager: McpManager,
    pub fake: Fake,
    pub notifications: Arc<Mutex<Vec<String>>>,
    pub dir: tempfile::TempDir,
}

pub fn entry(name: &str, config: Value) -> McpServerEntry {
    McpServerEntry::new(name, serde_json::from_value(config).unwrap(), "test")
}

/// A manager whose servers are fakes; `configure` adjusts its options before it starts.
pub fn setup(fake: Fake, loaded: LoadedMcpConfig, configure: impl FnOnce(&mut McpManagerOptions)) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let notifications: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut options = McpManagerOptions::new(dir.path());
    options.create_transport = fake.factory();
    options.credentials =
        Some(McpOAuthCredentialStore::with_backend(Arc::new(InMemoryAuthStorageBackend::default()), None));
    let seen = notifications.clone();
    options.notify = Some(Arc::new(move |message: &str, _: NotifyLevel| seen.lock().unwrap().push(message.into())));
    configure(&mut options);
    let manager = McpManager::new(options);
    manager.start(dir.path().into(), loaded);
    Setup { manager, fake, notifications, dir }
}

/// One server named `docs` with the given config.
pub fn docs(config: Value, options: FakeOptions) -> Setup {
    let loaded = LoadedMcpConfig { servers: vec![entry("docs", config)], ..LoadedMcpConfig::default() };
    setup(Fake::new(options), loaded, |_| {})
}
