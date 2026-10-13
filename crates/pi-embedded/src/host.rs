//! Host contracts: storage, credentials, networking, tools, workspace, compaction, lifecycle.
//! No host implementation is selected implicitly.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use pi_agent::StreamFn;
use pi_ai::{JsonObject, Message, Model};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::types::{
    Compaction, LifecycleState, NetworkRequest, Reconciliation, SearchMatch, SessionSnapshot, SessionSummary,
    ToolEffect, ToolInvocation, ToolResult, WorkspaceDocument,
};

/// Session checkpoints. `save` must atomically compare `expected_revision` (`None` creates)
/// and replace the checkpoint; later writes advance the revision by one.
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn load(&self, id: &str) -> anyhow::Result<Option<SessionSnapshot>>;
    async fn list(&self) -> anyhow::Result<Vec<SessionSummary>>;
    async fn save(&self, snapshot: &SessionSnapshot, expected_revision: Option<u64>) -> anyhow::Result<()>;
}

/// Provider secrets, resolved for every request and never persisted in sessions.
#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn get(&self, provider: &str) -> anyhow::Result<Option<String>>;
    async fn set(&self, provider: &str, secret: Option<String>) -> anyhow::Result<()>;
}

/// HTTP status and incrementally decoded UTF-8 body. Dropping the body closes the request.
pub struct NetworkResponse {
    pub status: u16,
    pub body: BoxStream<'static, anyhow::Result<String>>,
}

#[async_trait]
pub trait NetworkAdapter: Send + Sync {
    /// Cancellation must close the request, including during connection establishment.
    async fn request(&self, request: NetworkRequest, cancel: CancellationToken) -> anyhow::Result<NetworkResponse>;
}

/// Declared shape of a host tool (JSON compatible with upstream `EmbeddedTool` fields).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDeclaration {
    pub name: String,
    pub description: String,
    /// JSON Schema; arguments are validated before execution.
    pub parameters: Value,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub effect: ToolEffect,
    /// Request review even for a read effect. Every mutation is reviewed.
    #[serde(default)]
    pub approval: bool,
}

pub type ProgressFn = Arc<dyn Fn(ToolResult) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// Passed to a tool handler for one invocation.
#[derive(Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub invocation_id: String,
    pub cancel: CancellationToken,
    pub(crate) progress: ProgressFn,
}

impl ToolContext {
    pub fn new(
        session_id: impl Into<String>,
        invocation_id: impl Into<String>,
        cancel: CancellationToken,
        progress: ProgressFn,
    ) -> Self {
        Self { session_id: session_id.into(), invocation_id: invocation_id.into(), cancel, progress }
    }

    /// Checkpoint and publish a partial result.
    pub async fn progress(&self, result: ToolResult) -> anyhow::Result<()> {
        (self.progress)(result).await
    }
}

/// A host tool. Returning an error result asserts a known failed outcome; returning `Err`
/// from a mutation leaves its outcome uncertain and stops the batch.
#[async_trait]
pub trait EmbeddedTool: Send + Sync {
    fn declaration(&self) -> &ToolDeclaration;

    async fn execute(&self, args: JsonObject, context: ToolContext) -> anyhow::Result<ToolResult>;

    /// Decide an uncertain invocation from persisted evidence. The default cannot tell.
    async fn reconcile(
        &self,
        _invocation: ToolInvocation,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Reconciliation> {
        Ok(Reconciliation::Unknown)
    }
}

type ExecuteFn = Arc<dyn Fn(JsonObject, ToolContext) -> BoxFuture<'static, anyhow::Result<ToolResult>> + Send + Sync>;
type ReconcileFn =
    Arc<dyn Fn(ToolInvocation, CancellationToken) -> BoxFuture<'static, anyhow::Result<Reconciliation>> + Send + Sync>;

/// An [`EmbeddedTool`] built from closures.
#[derive(Clone)]
pub struct FnTool {
    declaration: ToolDeclaration,
    execute: ExecuteFn,
    reconcile: Option<ReconcileFn>,
}

impl FnTool {
    pub fn new<F, Fut>(declaration: ToolDeclaration, execute: F) -> Self
    where
        F: Fn(JsonObject, ToolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<ToolResult>> + Send + 'static,
    {
        Self { declaration, execute: Arc::new(move |args, context| Box::pin(execute(args, context))), reconcile: None }
    }

    pub fn with_reconcile<F, Fut>(mut self, reconcile: F) -> Self
    where
        F: Fn(ToolInvocation, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<Reconciliation>> + Send + 'static,
    {
        self.reconcile = Some(Arc::new(move |invocation, cancel| Box::pin(reconcile(invocation, cancel))));
        self
    }
}

#[async_trait]
impl EmbeddedTool for FnTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    async fn execute(&self, args: JsonObject, context: ToolContext) -> anyhow::Result<ToolResult> {
        (self.execute)(args, context).await
    }

    async fn reconcile(&self, invocation: ToolInvocation, cancel: CancellationToken) -> anyhow::Result<Reconciliation> {
        match &self.reconcile {
            Some(reconcile) => reconcile(invocation, cancel).await,
            None => Ok(Reconciliation::Unknown),
        }
    }
}

/// Summarizes older context. Failure or an empty summary leaves context unchanged.
#[async_trait]
pub trait Compactor: Send + Sync {
    async fn compact(
        &self,
        messages: Vec<Message>,
        model: Model,
        cancel: CancellationToken,
    ) -> anyhow::Result<Compaction>;
}

pub type LifecycleListener = Arc<dyn Fn(LifecycleState) + Send + Sync>;

/// Application lifecycle notifications; non-active states suspend open sessions.
pub trait Lifecycle: Send + Sync {
    /// Returns a function that removes the listener.
    fn subscribe(&self, listener: LifecycleListener) -> Box<dyn FnOnce() + Send>;
}

/// Editor or file workspace access. Implementations check the revision and apply an edit as
/// one serialized operation.
#[async_trait]
pub trait WorkspaceAdapter: Send + Sync {
    async fn read(&self, path: &str, cancel: CancellationToken) -> anyhow::Result<WorkspaceDocument>;
    async fn search(&self, query: &str, cancel: CancellationToken) -> anyhow::Result<Vec<SearchMatch>>;
    async fn edit(
        &self,
        path: &str,
        expected_revision: &str,
        text: &str,
        invocation_id: &str,
        cancel: CancellationToken,
    ) -> anyhow::Result<WorkspaceDocument>;
    async fn reconcile_edit(
        &self,
        invocation: ToolInvocation,
        cancel: CancellationToken,
    ) -> anyhow::Result<Reconciliation>;

    /// Whether [`WorkspaceAdapter::diagnostics`] is available; it adds the `diagnostics` tool.
    fn supports_diagnostics(&self) -> bool {
        false
    }

    async fn diagnostics(&self, _path: &str, _cancel: CancellationToken) -> anyhow::Result<Value> {
        anyhow::bail!("Workspace diagnostics are unavailable")
    }

    /// Whether [`WorkspaceAdapter::navigate`] is available; it adds the `navigate` tool.
    fn supports_navigation(&self) -> bool {
        false
    }

    async fn navigate(&self, _path: &str, _line: u64, _cancel: CancellationToken) -> anyhow::Result<Value> {
        anyhow::bail!("Workspace navigation is unavailable")
    }
}

pub type ListenerErrorFn = Arc<dyn Fn(String) + Send + Sync>;

/// Everything the runtime needs from its application.
#[derive(Clone)]
pub struct EmbeddedHost {
    pub store: Arc<dyn SessionStore>,
    /// Capabilities the host advertises; tools requiring others stay hidden.
    pub capabilities: Vec<String>,
    pub stream: StreamFn,
    pub tools: Vec<Arc<dyn EmbeddedTool>>,
    pub compactor: Option<Arc<dyn Compactor>>,
    /// Approximate JSON character threshold for automatic compaction.
    pub compact_at_characters: Option<usize>,
    pub lifecycle: Option<Arc<dyn Lifecycle>>,
    /// Receives listener panics and lifecycle suspension failures.
    pub listener_error: Option<ListenerErrorFn>,
}

impl EmbeddedHost {
    pub fn new(store: Arc<dyn SessionStore>, stream: StreamFn) -> Self {
        Self {
            store,
            capabilities: Vec::new(),
            stream,
            tools: Vec::new(),
            compactor: None,
            compact_at_characters: None,
            lifecycle: None,
            listener_error: None,
        }
    }
}
