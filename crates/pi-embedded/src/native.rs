//! The native host bridge (upstream `src/native.ts`): JSON commands in, JSON host calls and
//! events out. The C ABI in `ffi.rs` exposes it to Swift and other native hosts.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use pi_ai::{JsonObject, Model};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::host::{EmbeddedHost, ToolContext, ToolDeclaration};
use crate::native_host::{NativeCredentials, NativeNetwork, NativeStore, NativeTool, NativeWorkspace};
use crate::openai::{create_model_compactor, create_openai_stream};
use crate::runtime::{CreateSessionOptions, EmbeddedRuntime, lock};
use crate::session::EmbeddedSession;
use crate::types::{Reconciliation, ToolResult};
use crate::workspace::create_workspace_tools;

/// The application side of the bridge. Both methods may be called from any thread.
pub trait NativeHost: Send + Sync {
    /// A core request; answer later with [`NativeBridge::receive`] using the same `id`.
    /// `command.result` calls carry `{"value": ...}` or `{"error": "..."}` and need no answer.
    fn call(&self, id: &str, method: &str, payload: &str);
    /// A session event, or `{"version":1,"type":"host_error","error":"..."}`.
    fn event(&self, event: &str);
}

pub(crate) struct BridgeInner {
    host: Arc<dyn NativeHost>,
    handle: tokio::runtime::Handle,
    counter: AtomicU64,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    capabilities: Mutex<Vec<String>>,
    runtime: Mutex<Option<Arc<EmbeddedRuntime>>>,
    sessions: Mutex<HashMap<String, EmbeddedSession>>,
    pub tool_contexts: Mutex<HashMap<String, ToolContext>>,
}

impl BridgeInner {
    pub fn next_id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.counter.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// Ask the host and wait for its answer.
    pub async fn call<T: DeserializeOwned>(&self, method: &str, payload: impl Serialize) -> anyhow::Result<T> {
        let id = self.next_id("host");
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(id.clone(), sender);
        let payload = serde_json::to_string(&payload)?;
        self.host.call(&id, method, &payload);
        let value = receiver.await.map_err(|_| anyhow::anyhow!("Native bridge closed"))?;
        Ok(serde_json::from_value(value.map_err(|error| anyhow::anyhow!(error))?)?)
    }

    /// Send a host call without waiting for its answer.
    pub fn notify(self: &Arc<Self>, method: &'static str, payload: Value) {
        let bridge = self.clone();
        self.handle.spawn(async move {
            let _ = bridge.call::<Value>(method, payload).await;
        });
    }

    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        drop(self.handle.spawn(task));
    }
}

/// Dispatches host commands to an [`EmbeddedRuntime`] whose storage, credentials, networking,
/// workspace, and custom tools are host calls.
#[derive(Clone)]
pub struct NativeBridge {
    inner: Arc<BridgeInner>,
}

impl NativeBridge {
    /// Commands and host calls run on `handle`.
    pub fn new(host: Arc<dyn NativeHost>, handle: tokio::runtime::Handle) -> Self {
        let inner = BridgeInner {
            host,
            handle,
            counter: AtomicU64::new(0),
            pending: Mutex::default(),
            capabilities: Mutex::default(),
            runtime: Mutex::default(),
            sessions: Mutex::default(),
            tool_contexts: Mutex::default(),
        };
        Self { inner: Arc::new(inner) }
    }

    /// Deliver the host's answer to a call; `error` rejects it.
    pub fn receive(&self, id: &str, result_json: &str, error: Option<&str>) {
        let Some(sender) = lock(&self.inner.pending).remove(id) else { return };
        let result = match error {
            Some(error) => Err(error.to_string()),
            None => serde_json::from_str(result_json).map_err(|error| error.to_string()),
        };
        let _ = sender.send(result);
    }

    /// Run a command; its outcome arrives as a `command.result` host call with the same `id`.
    pub fn command(&self, id: &str, method: &str, payload_json: &str) {
        let (bridge, id, method, payload) =
            (self.inner.clone(), id.to_string(), method.to_string(), payload_json.to_string());
        self.inner.spawn(async move {
            let outcome = match serde_json::from_str::<Value>(&payload) {
                Ok(Value::Object(args)) => dispatch(&bridge, &method, args).await,
                Ok(_) => Err(anyhow::anyhow!("Command payload must be an object")),
                Err(error) => Err(error.into()),
            };
            let result = match outcome {
                Ok(value) => json!({"value": value}),
                Err(error) => json!({"error": error.to_string()}),
            };
            bridge.host.call(&id, "command.result", &result.to_string());
        });
    }
}

fn engine(bridge: &Arc<BridgeInner>) -> anyhow::Result<Arc<EmbeddedRuntime>> {
    let mut runtime = lock(&bridge.runtime);
    if let Some(runtime) = runtime.as_ref() {
        return Ok(runtime.clone());
    }
    let weak: Weak<BridgeInner> = Arc::downgrade(bridge);
    let stream = create_openai_stream(Arc::new(NativeNetwork(weak.clone())), Arc::new(NativeCredentials(weak.clone())));
    let mut host = EmbeddedHost::new(Arc::new(NativeStore(weak.clone())), stream.clone());
    host.capabilities = lock(&bridge.capabilities).clone();
    host.compactor = Some(create_model_compactor(stream));
    host.tools = create_workspace_tools(Arc::new(NativeWorkspace(weak.clone())));
    let reporter = weak;
    host.listener_error = Some(Arc::new(move |error| {
        if let Some(bridge) = reporter.upgrade() {
            bridge.host.event(&json!({"version": 1, "type": "host_error", "error": error}).to_string());
        }
    }));
    let created = Arc::new(EmbeddedRuntime::new(host)?);
    *runtime = Some(created.clone());
    Ok(created)
}

fn field<T: DeserializeOwned>(args: &JsonObject, key: &str) -> anyhow::Result<T> {
    Ok(serde_json::from_value(args.get(key).cloned().unwrap_or(Value::Null))?)
}

/// Upstream reads text arguments with `String(...)`.
fn text(args: &JsonObject, key: &str) -> String {
    match args.get(key) {
        Some(Value::String(value)) => value.clone(),
        Some(value) => value.to_string(),
        None => "undefined".into(),
    }
}

async fn dispatch(bridge: &Arc<BridgeInner>, method: &str, args: JsonObject) -> anyhow::Result<Value> {
    match method {
        "configure" => {
            if lock(&bridge.runtime).is_some() {
                anyhow::bail!("Configure before opening the runtime");
            }
            let capabilities: Vec<String> =
                field(&args, "capabilities").map_err(|_| anyhow::anyhow!("Invalid capabilities"))?;
            *lock(&bridge.capabilities) = capabilities;
            return Ok(Value::Null);
        }
        "list" => return Ok(serde_json::to_value(engine(bridge)?.list().await?)?),
        "create" | "restore" => {
            let runtime = engine(bridge)?;
            let id = text(&args, "id");
            let session = if method == "create" {
                let model: Model = field(&args, "model")?;
                let system_prompt = args.get("systemPrompt").and_then(Value::as_str).map(String::from);
                runtime.create(CreateSessionOptions { id: id.clone(), model, system_prompt }).await?
            } else {
                runtime.restore(&id).await?
            };
            lock(&bridge.sessions).insert(id, session.clone());
            let weak = Arc::downgrade(bridge);
            session.subscribe(move |event| {
                if let (Some(bridge), Ok(json)) = (weak.upgrade(), serde_json::to_string(event)) {
                    bridge.host.event(&json);
                }
            });
            return Ok(serde_json::to_value(session.snapshot())?);
        }
        "registerTool" => {
            let declaration: ToolDeclaration = serde_json::from_value(Value::Object(args))?;
            engine(bridge)?.register_tool(Arc::new(NativeTool { declaration, bridge: Arc::downgrade(bridge) }))?;
            return Ok(Value::Null);
        }
        "toolProgress" => {
            let key = json!([args.get("sessionId"), args.get("invocationId")]).to_string();
            let context = lock(&bridge.tool_contexts).get(&key).cloned();
            let context = context.ok_or_else(|| anyhow::anyhow!("Tool execution has settled"))?;
            context.progress(field::<ToolResult>(&args, "result")?).await?;
            return Ok(Value::Null);
        }
        _ => {}
    }
    let session = match args.get("id") {
        Some(Value::String(id)) => lock(&bridge.sessions).get(id).cloned(),
        _ => None,
    };
    let session = session.ok_or_else(|| anyhow::anyhow!("Session is not open"))?;
    match method {
        "snapshot" => return Ok(serde_json::to_value(session.snapshot())?),
        "submit" => session.submit(&text(&args, "text")).await?,
        "steer" => session.steer(&text(&args, "text")).await?,
        "followUp" => session.follow_up(&text(&args, "text")).await?,
        "cancel" => session.cancel().await?,
        "suspend" => session.suspend().await?,
        "resume" => session.resume().await?,
        "dispose" => {
            session.dispose().await?;
            lock(&bridge.sessions).remove(&text(&args, "id"));
        }
        "approve" => {
            session.approve(&text(&args, "invocationId"), args.get("approved") == Some(&Value::Bool(true))).await?
        }
        "reconcile" => {
            let decision: Option<Reconciliation> = field(&args, "outcome")?;
            session.reconcile(&text(&args, "invocationId"), decision).await?
        }
        "compact" => session.compact().await?,
        "setModel" => session.select_model(field(&args, "model")?).await?,
        _ => anyhow::bail!("Unknown command: {method}"),
    }
    Ok(Value::Null)
}
