//! JSON-RPC 2.0 over newline-delimited JSON (the TypeScript SDK's `ndJsonStream` and
//! `Connection`): requests run concurrently, notifications go out in order, and the agent's
//! own requests (`session/request_permission`) wait for the client's response.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::{
    CancelNotification, DeleteSessionRequest, Error, InitializeRequest, ListSessionsRequest, LoadSessionRequest,
    NewSessionRequest, PromptRequest, RequestPermissionRequest, RequestPermissionResponse, SessionNotification,
    SetSessionConfigOptionRequest, SetSessionModeRequest,
};
use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::AsyncRead;
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use super::agent::{AcpAgent, Handled};
use super::backend::AcpClient;
use super::errors::{invalid_params, method_not_found};
use crate::agent_session::lock;
use crate::rpc::RpcOutput;
use crate::rpc::jsonl::JsonlReader;

/// How long requests in flight may take to answer once the input ended.
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

type Pending = HashMap<i64, oneshot::Sender<Result<Value, Error>>>;

/// The client end of the connection: notifications and requests to the client.
pub struct ClientConnection {
    output: RpcOutput,
    pending: Mutex<Pending>,
    next_id: AtomicI64,
}

impl ClientConnection {
    pub fn new(output: RpcOutput) -> Arc<Self> {
        Arc::new(Self { output, pending: Mutex::new(HashMap::new()), next_id: AtomicI64::new(0) })
    }

    pub fn output(&self) -> &RpcOutput {
        &self.output
    }

    fn notify(&self, method: &str, params: Value) {
        self.output.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Send a request and wait for the client's answer.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(id, sender);
        self.output.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        receiver.await.unwrap_or_else(|_| Err(Error::internal_error().data(json!("ACP connection closed"))))
    }

    /// Route a response from the client to its waiting request.
    fn resolve(&self, message: &Value) {
        let Some(id) = message["id"].as_i64() else { return };
        let Some(sender) = lock(&self.pending).remove(&id) else { return };
        let result = match message.get("error") {
            Some(error) => Err(serde_json::from_value(error.clone()).unwrap_or_else(|_| Error::internal_error())),
            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = sender.send(result);
    }

    /// Fail every waiting request once the client is gone.
    fn close(&self) {
        lock(&self.pending).clear();
    }
}

#[async_trait]
impl AcpClient for ClientConnection {
    fn session_update(&self, notification: SessionNotification) {
        self.notify("session/update", serde_json::to_value(notification).unwrap_or(Value::Null));
    }

    async fn request_permission(&self, request: RequestPermissionRequest) -> Result<RequestPermissionResponse, Error> {
        let params = serde_json::to_value(request).unwrap_or(Value::Null);
        let result = self.request("session/request_permission", params).await?;
        serde_json::from_value(result).map_err(|error| Error::internal_error().data(json!(error.to_string())))
    }
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, Error> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(|error| invalid_params(error.to_string()))
}

fn to_value(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

type Reply = Result<(Value, Option<BoxFuture<'static, ()>>), Error>;

fn plain(value: Value) -> Reply {
    Ok((value, None))
}

fn handled(result: Result<Handled<Value>, Error>) -> Reply {
    result.map(|handled| (handled.response, handled.after))
}

/// Answer one request from the client.
pub async fn dispatch(agent: &AcpAgent, method: &str, params: Value) -> Reply {
    match method {
        "initialize" => plain(to_value(agent.initialize(parse::<InitializeRequest>(params)?))),
        "authenticate" => plain(to_value(agent.authenticate())),
        "session/new" => handled(agent.new_session(parse::<NewSessionRequest>(params)?).await),
        "session/load" => handled(agent.load_session(parse::<LoadSessionRequest>(params)?).await),
        "session/list" => plain(to_value(agent.list_sessions(parse::<ListSessionsRequest>(params)?))),
        "session/delete" => plain(to_value(agent.delete_session(parse::<DeleteSessionRequest>(params)?))),
        "session/prompt" => plain(to_value(agent.prompt(parse::<PromptRequest>(params)?).await?)),
        "session/set_mode" => plain(to_value(agent.set_session_mode(parse::<SetSessionModeRequest>(params)?).await?)),
        "session/set_config_option" => {
            let request = parse::<SetSessionConfigOptionRequest>(params)?;
            plain(to_value(agent.set_session_config_option(request).await?))
        }
        "session/set_model" => {
            let field = |key: &str| params[key].as_str().map(String::from);
            let (Some(session_id), Some(model_id)) = (field("sessionId"), field("modelId")) else {
                return Err(invalid_params("Expected sessionId and modelId strings"));
            };
            plain(agent.set_session_model(&session_id, &model_id).await?)
        }
        _ => Err(method_not_found(method)),
    }
}

async fn handle_request(agent: Arc<AcpAgent>, client: Arc<ClientConnection>, id: Value, method: String, params: Value) {
    let reply = dispatch(&agent, &method, params).await;
    let (message, after) = match reply {
        Ok((result, after)) => (json!({"jsonrpc": "2.0", "id": id, "result": result}), after),
        Err(error) => {
            eprintln!("Error handling request {method}: {}", error.message);
            (json!({"jsonrpc": "2.0", "id": id, "error": to_value(error)}), None)
        }
    };
    client.output.send(&message);
    if let Some(after) = after {
        after.await;
    }
}

/// A record that is neither a request, a notification, nor a response.
#[derive(Debug)]
pub struct InvalidMessage;

/// One record from the client: work to run for requests and notifications, `Err` when the
/// record cannot be routed.
pub fn route(
    agent: &Arc<AcpAgent>,
    client: &Arc<ClientConnection>,
    message: Value,
) -> Result<Option<BoxFuture<'static, ()>>, InvalidMessage> {
    let method = message.get("method").and_then(Value::as_str).map(String::from);
    let id = message.get("id").cloned();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    Ok(match (method, id) {
        (Some(method), Some(id)) => Some(Box::pin(handle_request(agent.clone(), client.clone(), id, method, params))),
        (Some(method), None) if method == "session/cancel" => match parse::<CancelNotification>(params) {
            Ok(cancel) => {
                let agent = agent.clone();
                Some(Box::pin(async move { agent.cancel(cancel).await }))
            }
            Err(error) => {
                eprintln!("Error handling notification {method}: {}", error.message);
                None
            }
        },
        (Some(_), None) => None,
        (None, Some(_)) => {
            client.resolve(&message);
            None
        }
        (None, None) => return Err(InvalidMessage),
    })
}

/// Read records until the input ends or the output fails; returns the exit code. At the end,
/// running prompts are cancelled and requests in flight get a short time to answer.
pub async fn read_loop(
    agent: &Arc<AcpAgent>,
    client: &Arc<ClientConnection>,
    input: impl AsyncRead + Unpin,
    stop: impl std::future::Future<Output = i32>,
) -> i32 {
    let mut reader = JsonlReader::new(input);
    let closed = client.output.closed();
    let mut tasks = JoinSet::new();
    tokio::pin!(stop);
    let code = loop {
        tokio::select! {
            line = reader.next_line() => match line {
                Ok(Some(line)) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let routed = serde_json::from_str::<Value>(line)
                        .map_err(|error| eprintln!("Failed to parse JSON message: {line}: {error}"))
                        .and_then(|message| route(agent, client, message).map_err(|InvalidMessage| {
                            eprintln!("Invalid ACP message: {line}");
                        }));
                    if let Ok(Some(task)) = routed {
                        tasks.spawn(task);
                    }
                }
                Ok(None) => break 0,
                Err(error) => {
                    eprintln!("Error: could not read ACP input: {error}");
                    break 0;
                }
            },
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            _ = closed.cancelled() => break 0,
            code = &mut stop => break code,
        }
    };
    agent.cancel_all().await;
    let drain = async { while tasks.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, drain).await;
    tasks.abort_all();
    client.close();
    code
}
