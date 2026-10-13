//! Host adapters whose operations are native bridge calls (store, credentials, network,
//! workspace, and host-registered tools). Method names and payloads match upstream.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use futures::StreamExt;
use pi_ai::JsonObject;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::host::{
    CredentialStore, EmbeddedTool, NetworkAdapter, NetworkResponse, SessionStore, ToolContext, ToolDeclaration,
    WorkspaceAdapter,
};
use crate::native::BridgeInner;
use crate::runtime::lock;
use crate::types::{
    NetworkRequest, Reconciliation, SearchMatch, SessionSnapshot, SessionSummary, ToolInvocation, ToolResult,
    WorkspaceDocument,
};

fn bridge(weak: &Weak<BridgeInner>) -> anyhow::Result<Arc<BridgeInner>> {
    weak.upgrade().ok_or_else(|| anyhow::anyhow!("Native bridge closed"))
}

async fn call<T: DeserializeOwned>(weak: &Weak<BridgeInner>, method: &str, payload: Value) -> anyhow::Result<T> {
    bridge(weak)?.call(method, payload).await
}

pub(crate) struct NativeStore(pub Weak<BridgeInner>);

#[async_trait]
impl SessionStore for NativeStore {
    async fn load(&self, id: &str) -> anyhow::Result<Option<SessionSnapshot>> {
        call(&self.0, "store.load", json!({"id": id})).await
    }

    async fn list(&self) -> anyhow::Result<Vec<SessionSummary>> {
        call(&self.0, "store.list", json!({})).await
    }

    async fn save(&self, snapshot: &SessionSnapshot, expected_revision: Option<u64>) -> anyhow::Result<()> {
        let _: Value =
            call(&self.0, "store.save", json!({"snapshot": snapshot, "expectedRevision": expected_revision})).await?;
        Ok(())
    }
}

pub(crate) struct NativeCredentials(pub Weak<BridgeInner>);

#[async_trait]
impl CredentialStore for NativeCredentials {
    async fn get(&self, provider: &str) -> anyhow::Result<Option<String>> {
        call(&self.0, "credential.get", json!({"provider": provider})).await
    }

    async fn set(&self, provider: &str, secret: Option<String>) -> anyhow::Result<()> {
        let _: Value = call(&self.0, "credential.set", json!({"provider": provider, "secret": secret})).await?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct Opened {
    status: u16,
}

#[derive(Deserialize)]
struct Chunk {
    done: bool,
    #[serde(default)]
    value: Option<String>,
}

/// Closes the host stream once: on completion, failure, cancellation, or drop.
struct HostStream {
    bridge: Weak<BridgeInner>,
    id: String,
    closed: std::sync::atomic::AtomicBool,
}

impl HostStream {
    fn close(&self) {
        if !self.closed.swap(true, std::sync::atomic::Ordering::SeqCst)
            && let Some(bridge) = self.bridge.upgrade()
        {
            bridge.notify("network.cancel", json!({"id": self.id}));
        }
    }
}

impl Drop for HostStream {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) struct NativeNetwork(pub Weak<BridgeInner>);

#[async_trait]
impl NetworkAdapter for NativeNetwork {
    async fn request(&self, request: NetworkRequest, cancel: CancellationToken) -> anyhow::Result<NetworkResponse> {
        let bridge = bridge(&self.0)?;
        if cancel.is_cancelled() {
            anyhow::bail!("Request cancelled");
        }
        let id = bridge.next_id("network");
        let stream = Arc::new(HostStream { bridge: self.0.clone(), id: id.clone(), closed: Default::default() });
        let watcher = stream.clone();
        let (finished, watch) = (CancellationToken::new(), cancel.clone());
        let done = finished.clone();
        bridge.spawn(async move {
            tokio::select! { _ = watch.cancelled() => watcher.close(), _ = done.cancelled() => {} }
        });
        let opened: Opened = match bridge.call("network.open", json!({"id": id, "request": request})).await {
            Ok(opened) => opened,
            Err(error) => {
                stream.close();
                finished.cancel();
                return Err(error);
            }
        };
        if cancel.is_cancelled() {
            stream.close();
            anyhow::bail!("Request cancelled");
        }
        let state = (stream, cancel, finished.drop_guard());
        let body = futures::stream::unfold(Some(state), |state| async move {
            let (stream, cancel, guard) = state?;
            if cancel.is_cancelled() {
                stream.close();
                return Some((Err(anyhow::anyhow!("Request cancelled")), None));
            }
            let next: anyhow::Result<Chunk> = call(&stream.bridge, "network.next", json!({"id": stream.id})).await;
            match next {
                Ok(Chunk { done: false, value }) => {
                    Some((Ok(value.unwrap_or_default()), Some((stream, cancel, guard))))
                }
                Ok(Chunk { done: true, .. }) => {
                    if !stream.closed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        let _: anyhow::Result<Value> =
                            call(&stream.bridge, "network.cancel", json!({"id": stream.id})).await;
                    }
                    None
                }
                Err(error) => {
                    stream.close();
                    Some((Err(error), None))
                }
            }
        });
        Ok(NetworkResponse { status: opened.status, body: body.boxed() })
    }
}

pub(crate) struct NativeWorkspace(pub Weak<BridgeInner>);

#[async_trait]
impl WorkspaceAdapter for NativeWorkspace {
    async fn read(&self, path: &str, _: CancellationToken) -> anyhow::Result<WorkspaceDocument> {
        call(&self.0, "workspace.read", json!({"path": path})).await
    }

    async fn search(&self, query: &str, _: CancellationToken) -> anyhow::Result<Vec<SearchMatch>> {
        call(&self.0, "workspace.search", json!({"query": query})).await
    }

    async fn edit(
        &self,
        path: &str,
        expected: &str,
        text: &str,
        invocation_id: &str,
        _: CancellationToken,
    ) -> anyhow::Result<WorkspaceDocument> {
        let payload = json!({"path": path, "expectedRevision": expected, "text": text, "invocationId": invocation_id});
        call(&self.0, "workspace.edit", payload).await
    }

    async fn reconcile_edit(&self, invocation: ToolInvocation, _: CancellationToken) -> anyhow::Result<Reconciliation> {
        call(&self.0, "workspace.reconcileEdit", json!({"invocation": invocation})).await
    }

    fn supports_diagnostics(&self) -> bool {
        true
    }

    async fn diagnostics(&self, path: &str, _: CancellationToken) -> anyhow::Result<Value> {
        call(&self.0, "workspace.diagnostics", json!({"path": path})).await
    }

    fn supports_navigation(&self) -> bool {
        true
    }

    async fn navigate(&self, path: &str, line: u64, _: CancellationToken) -> anyhow::Result<Value> {
        call(&self.0, "workspace.navigate", json!({"path": path, "line": line})).await
    }
}

/// A tool registered by the host; execution, cancellation, and reconciliation are host calls.
pub(crate) struct NativeTool {
    pub declaration: ToolDeclaration,
    pub bridge: Weak<BridgeInner>,
}

#[async_trait]
impl EmbeddedTool for NativeTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    async fn execute(&self, args: JsonObject, context: ToolContext) -> anyhow::Result<ToolResult> {
        let bridge = bridge(&self.bridge)?;
        let key = json!([context.session_id, context.invocation_id]).to_string();
        lock(&bridge.tool_contexts).insert(key.clone(), context.clone());
        let ids = json!({"sessionId": context.session_id, "invocationId": context.invocation_id});
        let (watch, finished) = (context.cancel.clone(), CancellationToken::new());
        let (done, notifier, cancel_payload) = (finished.clone(), bridge.clone(), ids.clone());
        bridge.spawn(async move {
            tokio::select! {
                _ = watch.cancelled() => notifier.notify("tool.cancel", cancel_payload),
                _ = done.cancelled() => {}
            }
        });
        let mut payload = ids;
        payload["name"] = json!(self.declaration.name);
        payload["args"] = Value::Object(args);
        let result = bridge.call("tool.execute", payload).await;
        finished.cancel();
        lock(&bridge.tool_contexts).remove(&key);
        result
    }

    async fn reconcile(&self, invocation: ToolInvocation, _: CancellationToken) -> anyhow::Result<Reconciliation> {
        call(&self.bridge, "tool.reconcile", json!({"invocation": invocation})).await
    }
}
