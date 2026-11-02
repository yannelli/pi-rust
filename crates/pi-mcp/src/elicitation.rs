//! Elicitation (`elicitation/create`): a server asks the user for form data (`requestedSchema`)
//! or to open a URL (`url`, `elicitationId`). The client answers `accept`, `decline`, or `cancel`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::client::{McpClient, RequestHandler};
use crate::error::{McpError, codes};
use crate::log::McpServerLog;

pub const ELICITATION_METHOD: &str = "elicitation/create";
pub const ELICITATION_COMPLETE_NOTIFICATION: &str = "notifications/elicitation/complete";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElicitationMode {
    Form,
    Url,
}

impl ElicitationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Url => "url",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

impl ElicitationAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "accept" => Some(Self::Accept),
            "decline" => Some(Self::Decline),
            "cancel" => Some(Self::Cancel),
            _ => None,
        }
    }
}

/// An `elicitation/create` request of the server `server`.
#[derive(Clone, Debug, PartialEq)]
pub struct ElicitationRequest {
    pub server: String,
    pub message: String,
    pub mode: ElicitationMode,
    /// URL mode only.
    pub url: Option<String>,
    /// URL mode only.
    pub elicitation_id: Option<String>,
    /// Form mode only: a flat object schema with primitive properties.
    pub requested_schema: Option<Value>,
}

impl ElicitationRequest {
    /// Parse the request params; errors are `-32602` (invalid params).
    pub fn parse(server: &str, params: Option<&Value>) -> Result<Self, McpError> {
        let invalid = |message: &str| McpError::rpc(codes::INVALID_PARAMS, message);
        let params = params.and_then(Value::as_object).ok_or_else(|| invalid("Invalid elicitation params"))?;
        let message = params.get("message").and_then(Value::as_str).unwrap_or_default().to_string();
        let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_string);
        match params.get("mode").and_then(Value::as_str).unwrap_or("form") {
            "form" => {
                let schema = params.get("requestedSchema").filter(|schema| schema.is_object()).cloned();
                let schema = schema.unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                Ok(Self {
                    server: server.into(),
                    message,
                    mode: ElicitationMode::Form,
                    url: None,
                    elicitation_id: None,
                    requested_schema: Some(schema),
                })
            }
            "url" => {
                let url = text("url").filter(|url| url::Url::parse(url).is_ok());
                let url = url.ok_or_else(|| invalid("URL elicitation needs a valid url"))?;
                let id = text("elicitationId").ok_or_else(|| invalid("URL elicitation needs an elicitationId"))?;
                Ok(Self {
                    server: server.into(),
                    message,
                    mode: ElicitationMode::Url,
                    url: Some(url),
                    elicitation_id: Some(id),
                    requested_schema: None,
                })
            }
            mode => Err(invalid(&format!("Unsupported elicitation mode: {mode}"))),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ElicitationResponse {
    pub action: ElicitationAction,
    /// Form mode `accept` only.
    pub content: Option<Map<String, Value>>,
}

impl ElicitationResponse {
    pub fn accept(content: Option<Map<String, Value>>) -> Self {
        Self { action: ElicitationAction::Accept, content }
    }

    pub fn decline() -> Self {
        Self { action: ElicitationAction::Decline, content: None }
    }

    pub fn cancel() -> Self {
        Self { action: ElicitationAction::Cancel, content: None }
    }

    /// `{ action, content? }`, as the MCP result and the extension events carry it.
    pub fn to_json(&self) -> Value {
        let mut value = json!({ "action": self.action.as_str() });
        if let Some(content) = &self.content {
            value["content"] = Value::Object(content.clone());
        }
        value
    }

    /// Parse `{ action, content? }`; `None` when `action` is missing or unknown.
    pub fn from_json(value: &Value) -> Option<Self> {
        let action = ElicitationAction::parse(value.get("action")?.as_str()?)?;
        let content = value.get("content").and_then(Value::as_object).cloned();
        Some(Self { action, content })
    }
}

/// Asks the user. The token is cancelled when the server cancels the request.
pub type ElicitFn =
    Arc<dyn Fn(ElicitationRequest, CancellationToken) -> BoxFuture<'static, ElicitationResponse> + Send + Sync>;
/// Called with the server name and the `elicitationId` of a completed URL elicitation.
pub type ElicitationCompleteFn = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// How a client answers elicitation requests. Without it, pi does not announce the capability.
#[derive(Clone)]
pub struct McpElicitation {
    pub elicit: ElicitFn,
    /// Called for `notifications/elicitation/complete` of URL elicitations the user accepted.
    pub on_complete: Option<ElicitationCompleteFn>,
}

impl McpElicitation {
    pub fn new(elicit: ElicitFn) -> Self {
        Self { elicit, on_complete: None }
    }

    /// The `elicitation` client capability: form and URL mode.
    pub fn capability() -> Value {
        json!({ "form": {}, "url": {} })
    }

    /// Answer `elicitation/create` on `client` and watch for completions. Call before `connect`.
    pub fn install(&self, client: &McpClient, server: &str, log: Option<Arc<McpServerLog>>) {
        let accepted: Arc<Mutex<HashSet<String>>> = Arc::default();
        client.set_request_handler(ELICITATION_METHOD, self.handler(server, log, accepted.clone()));
        let (server, on_complete) = (server.to_string(), self.on_complete.clone());
        client.on_notification(ELICITATION_COMPLETE_NOTIFICATION, move |params| {
            let id = params.and_then(|params| params.get("elicitationId")).and_then(Value::as_str);
            // Unknown or already completed ids are ignored, as the spec requires.
            let known =
                id.is_some_and(|id| accepted.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(id));
            if let (true, Some(id), Some(on_complete)) = (known, id, &on_complete) {
                on_complete(&server, id);
            }
        });
    }

    fn handler(
        &self,
        server: &str,
        log: Option<Arc<McpServerLog>>,
        accepted: Arc<Mutex<HashSet<String>>>,
    ) -> RequestHandler {
        let (elicit, server) = (self.elicit.clone(), server.to_string());
        Arc::new(move |params, cancel| {
            let (elicit, server, log, accepted) = (elicit.clone(), server.clone(), log.clone(), accepted.clone());
            async move {
                let request = ElicitationRequest::parse(&server, params.as_ref())?;
                let (mode, schema, id) =
                    (request.mode, request.requested_schema.clone(), request.elicitation_id.clone());
                let mut response = elicit(request, cancel).await;
                if response.action != ElicitationAction::Accept {
                    response.content = None;
                } else if mode == ElicitationMode::Url {
                    response.content = None;
                    if let Some(id) = id {
                        accepted.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(id);
                    }
                } else {
                    let content = response.content.get_or_insert_with(Map::new);
                    if let Err(error) = validate_content(schema.as_ref(), content) {
                        if let Some(log) = &log {
                            let data = format!("elicitation answer declined: {error}");
                            log.write(&server, Some(&json!({ "level": "warning", "logger": "pi", "data": data })));
                        }
                        response = ElicitationResponse::decline();
                    }
                }
                Ok(response.to_json())
            }
            .boxed()
        })
    }
}

/// The allowed values of an enum property: `enum`, `oneOf`/`anyOf` `const`s, or array `items` of those.
pub fn enum_values(schema: &Value) -> Option<Vec<String>> {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return Some(values.iter().filter_map(Value::as_str).map(str::to_string).collect());
    }
    let options = schema.get("oneOf").or_else(|| schema.get("anyOf")).and_then(Value::as_array)?;
    Some(options.iter().filter_map(|option| option.get("const")?.as_str().map(str::to_string)).collect())
}

fn check_value(name: &str, property: &Value, value: &Value) -> Result<(), String> {
    let kind = property.get("type").and_then(Value::as_str).unwrap_or("string");
    let valid = match kind {
        "string" => value
            .as_str()
            .is_some_and(|text| enum_values(property).is_none_or(|values| values.iter().any(|v| v == text))),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|number| number.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "array" => {
            let allowed = property.get("items").and_then(enum_values);
            value.as_array().is_some_and(|items| {
                items.iter().all(|item| {
                    item.as_str()
                        .is_some_and(|text| allowed.as_ref().is_none_or(|values| values.iter().any(|v| v == text)))
                })
            })
        }
        _ => true,
    };
    if valid { Ok(()) } else { Err(format!("\"{name}\" is not a valid {kind}")) }
}

/// Check `content` against a form schema: required fields, known fields, and primitive types.
pub fn validate_content(schema: Option<&Value>, content: &Map<String, Value>) -> Result<(), String> {
    let empty = Map::new();
    let properties = schema.and_then(|schema| schema.get("properties")).and_then(Value::as_object).unwrap_or(&empty);
    let required = schema.and_then(|schema| schema.get("required")).and_then(Value::as_array);
    for name in required.into_iter().flatten().filter_map(Value::as_str) {
        if !content.contains_key(name) {
            return Err(format!("\"{name}\" is required"));
        }
    }
    for (name, value) in content {
        let property = properties.get(name).ok_or_else(|| format!("\"{name}\" is not in the requested schema"))?;
        check_value(name, property, value)?;
    }
    Ok(())
}
