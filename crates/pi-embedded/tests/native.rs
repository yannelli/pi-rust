//! Port of upstream `test/native.test.mjs`: the native bridge driven by a host that answers
//! storage, credential, network, and workspace calls.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use pi_embedded::{NativeBridge, NativeHost};
use serde_json::{Value, json};
use tokio::sync::oneshot;

type Delta = Arc<dyn Fn(usize, &Value) -> Value + Send + Sync>;

#[derive(Default)]
struct HostState {
    checkpoints: Arc<Mutex<BTreeMap<String, Value>>>,
    events: Mutex<Vec<Value>>,
    requests: Mutex<Vec<Value>>,
    commands: Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    streams: Mutex<HashMap<String, Vec<String>>>,
    command_id: AtomicUsize,
    opens: AtomicUsize,
    edits: AtomicUsize,
    document: Mutex<Value>,
    delta: Option<Delta>,
    bridge: OnceLock<NativeBridge>,
}

#[derive(Clone)]
struct Host(Arc<HostState>);

fn model() -> Value {
    json!({"id": "fixture", "name": "Fixture", "api": "openai-completions", "provider": "openai",
        "baseUrl": "https://provider.test/v1", "input": ["text"],
        "cost": {"input": 1, "output": 1, "cacheRead": 1, "cacheWrite": 0},
        "reasoning": false, "maxTokens": 1024, "contextWindow": 8192})
}

impl Host {
    fn new(checkpoints: Option<Arc<Mutex<BTreeMap<String, Value>>>>, delta: Option<Delta>) -> Self {
        let state = HostState {
            checkpoints: checkpoints.unwrap_or_default(),
            document: Mutex::new(json!({"path": "notes.txt", "text": "before", "revision": "rev-0"})),
            delta,
            ..HostState::default()
        };
        let host = Self(Arc::new(state));
        let bridge = NativeBridge::new(Arc::new(host.clone()), tokio::runtime::Handle::current());
        let _ = host.0.bridge.set(bridge);
        host
    }

    async fn command(&self, method: &str, payload: Value) -> Result<Value, String> {
        let id = format!("command-{}", self.0.command_id.fetch_add(1, Ordering::SeqCst) + 1);
        let (sender, receiver) = oneshot::channel();
        self.0.commands.lock().unwrap().insert(id.clone(), sender);
        self.0.bridge.get().unwrap().command(&id, method, &payload.to_string());
        receiver.await.unwrap()
    }

    fn answer(&self, method: &str, args: Value) -> Result<Value, String> {
        let state = &self.0;
        Ok(match method {
            "store.save" => {
                let mut checkpoints = state.checkpoints.lock().unwrap();
                let id = args["snapshot"]["id"].as_str().unwrap().to_string();
                let existing = checkpoints.get(&id).map(|snapshot| snapshot["revision"].clone()).unwrap_or(Value::Null);
                if existing != args["expectedRevision"] {
                    return Err("revision mismatch".into());
                }
                checkpoints.insert(id, args["snapshot"].clone());
                Value::Null
            }
            "store.load" => {
                state.checkpoints.lock().unwrap().get(args["id"].as_str().unwrap()).cloned().unwrap_or(Value::Null)
            }
            "store.list" => Value::Array(
                state
                    .checkpoints
                    .lock()
                    .unwrap()
                    .values()
                    .map(|s| json!({"id": s["id"], "status": s["status"], "revision": s["revision"]}))
                    .collect(),
            ),
            "credential.get" => json!("fixture-secret"),
            "network.open" => {
                state.requests.lock().unwrap().push(args["request"].clone());
                let opens = state.opens.fetch_add(1, Ordering::SeqCst) + 1;
                let body: Value = serde_json::from_str(args["request"]["body"].as_str().unwrap()).unwrap();
                let delta = match &state.delta {
                    Some(delta) => delta(opens, &body["messages"]),
                    None => json!({"content": "hello"}),
                };
                let finish = if delta.get("tool_calls").is_some() { "tool_calls" } else { "stop" };
                let wire = format!(
                    "data: {}\r\n\r\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    json!({"choices": [{"index": 0, "delta": delta, "finish_reason": null}]}),
                    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]}),
                    json!({"choices": [], "usage": {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15}}),
                );
                let pieces =
                    wire.as_bytes().chunks(13).map(|piece| String::from_utf8(piece.to_vec()).unwrap()).collect();
                state.streams.lock().unwrap().insert(args["id"].as_str().unwrap().into(), pieces);
                json!({"id": args["id"], "status": 200})
            }
            "network.next" => {
                let mut streams = state.streams.lock().unwrap();
                match streams.get_mut(args["id"].as_str().unwrap()).filter(|pieces| !pieces.is_empty()) {
                    Some(pieces) => json!({"done": false, "value": pieces.remove(0)}),
                    None => json!({"done": true}),
                }
            }
            "network.cancel" => {
                state.streams.lock().unwrap().remove(args["id"].as_str().unwrap());
                Value::Null
            }
            "workspace.read" => state.document.lock().unwrap().clone(),
            "workspace.search" => {
                json!([{"path": "notes.txt", "line": 1, "text": state.document.lock().unwrap()["text"]}])
            }
            "workspace.edit" => {
                let mut document = state.document.lock().unwrap();
                let approved = state
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| e["type"] == "approval_decision" && e["approved"] == true);
                if args["expectedRevision"] != document["revision"]
                    || args["invocationId"] != json!(r#"["session","edit-call"]"#)
                    || !approved
                {
                    return Err("unexpected edit".into());
                }
                *document = json!({"path": args["path"], "text": args["text"], "revision": "rev-1"});
                state.edits.fetch_add(1, Ordering::SeqCst);
                document.clone()
            }
            "tool.execute" => json!({"content": [{"type": "text", "text": "custom"}], "details": null}),
            _ => return Err(format!("Unexpected host method: {method}")),
        })
    }
}

impl NativeHost for Host {
    fn call(&self, id: &str, method: &str, payload: &str) {
        let args: Value = serde_json::from_str(payload).unwrap();
        if method == "command.result" {
            let sender = self.0.commands.lock().unwrap().remove(id).unwrap();
            let result = match args.get("error") {
                Some(error) => Err(error.as_str().unwrap().to_string()),
                None => Ok(args["value"].clone()),
            };
            let _ = sender.send(result);
            return;
        }
        let (host, id, method) = (self.clone(), id.to_string(), method.to_string());
        tokio::spawn(async move {
            let bridge = host.0.bridge.get().unwrap();
            match host.answer(&method, args) {
                Ok(value) => bridge.receive(&id, &value.to_string(), None),
                Err(error) => bridge.receive(&id, "null", Some(&error)),
            }
        });
    }

    fn event(&self, event: &str) {
        let event: Value = serde_json::from_str(event).unwrap();
        if event["type"] == "approval" {
            let (host, session, invocation) =
                (self.clone(), event["sessionId"].clone(), event["invocation"]["id"].clone());
            tokio::spawn(async move {
                host.command("approve", json!({"id": session, "invocationId": invocation, "approved": true})).await
            });
        }
        self.0.events.lock().unwrap().push(event);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_bridge_runs_sessions_through_host_calls_and_restores_them() {
    let native = Host::new(None, None);
    native.command("configure", json!({"capabilities": []})).await.unwrap();
    native.command("create", json!({"id": "session", "model": model()})).await.unwrap();
    native.command("submit", json!({"id": "session", "text": "hello"})).await.unwrap();
    let snapshot = native.command("snapshot", json!({"id": "session"})).await.unwrap();
    assert_eq!(snapshot["status"], "idle");
    assert_eq!(snapshot["messages"].as_array().unwrap().last().unwrap()["content"][0]["text"], "hello");
    assert_eq!(snapshot["usage"][0]["totalTokens"], 15);
    assert!(native.0.streams.lock().unwrap().is_empty());
    assert!(!snapshot.to_string().contains("fixture-secret"));
    assert_eq!(native.0.requests.lock().unwrap()[0]["headers"]["Authorization"], "Bearer fixture-secret");
    let events = native.0.events.lock().unwrap().clone();
    assert!(events.iter().all(|event| event["version"] == 1));
    assert!(events.windows(2).all(|pair| pair[1]["sequence"].as_u64() > pair[0]["sequence"].as_u64()));
    native.command("suspend", json!({"id": "session"})).await.unwrap();
    native.command("followUp", json!({"id": "session", "text": "queued"})).await.unwrap();
    let error = native.command("snapshot", json!({"id": "missing"})).await.unwrap_err();
    assert_eq!(error, "Session is not open");

    let reopened = Host::new(Some(native.0.checkpoints.clone()), None);
    reopened.command("configure", json!({"capabilities": []})).await.unwrap();
    reopened.command("restore", json!({"id": "session"})).await.unwrap();
    reopened.command("resume", json!({"id": "session"})).await.unwrap();
    let restored = reopened.command("snapshot", json!({"id": "session"})).await.unwrap();
    assert_eq!(restored["followUps"].as_array().unwrap().len(), 0);
    let users: Vec<&Value> = restored["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "user").collect();
    assert_eq!(users.last().unwrap()["content"], "queued");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_bridge_advertises_capability_tools_validates_schemas_and_reviews_file_edits() {
    let call = |id: &str, name: &str, args: Value| json!({"tool_calls": [{"index": 0, "id": id, "function": {"name": name, "arguments": args.to_string()}}]});
    let delta: Delta = Arc::new(move |request, _| match request {
        1 => call("read-call", "read_file", json!({"path": "notes.txt"})),
        2 => call("search-call", "search_files", json!({"query": "before"})),
        3 => call("edit-call", "edit_file", json!({"path": "notes.txt", "expectedRevision": "rev-0", "text": "after"})),
        _ => json!({"content": "done"}),
    });
    let native = Host::new(None, Some(delta));
    let capabilities = json!({"capabilities": ["workspace.read", "workspace.search", "workspace.edit"]});
    native.command("configure", capabilities).await.unwrap();
    native.command("create", json!({"id": "session", "model": model()})).await.unwrap();
    native.command("submit", json!({"id": "session", "text": "read, search, then edit"})).await.unwrap();
    let snapshot = native.command("snapshot", json!({"id": "session"})).await.unwrap();
    assert_eq!(snapshot["status"], "idle");
    assert_eq!(native.0.edits.load(Ordering::SeqCst), 1);
    let invocations = snapshot["invocations"].as_array().unwrap();
    assert_eq!(invocations.len(), 3);
    assert!(invocations.iter().all(|invocation| invocation["phase"] == "completed"));
    let results = snapshot["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "toolResult").count();
    assert_eq!(results, 3);
    let body: Value = serde_json::from_str(native.0.requests.lock().unwrap()[0]["body"].as_str().unwrap()).unwrap();
    let names: Vec<&str> =
        body["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["read_file", "search_files", "edit_file"]);
    let configured = native.command("configure", json!({"capabilities": []})).await.unwrap_err();
    assert_eq!(configured, "Configure before opening the runtime");
}
