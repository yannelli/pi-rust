//! MCP elicitation (pi-rust): `mcp_elicitation` handlers can answer a server request first; else
//! pi asks the user with dialogs, and `mcp_elicitation_result` handlers can replace the answer.

use std::sync::Arc;

use futures::FutureExt;
use pi_mcp::elicitation::enum_values;
use pi_mcp::{ElicitationMode, ElicitationRequest, ElicitationResponse, McpElicitation, NotifyLevel};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent_session::AgentSession;
use crate::extension_ui::{DialogOptions, ExtensionUi};

const RESPOND: &str = "Respond";
const ACCEPT: &str = "Accept";
const DECLINE: &str = "Decline";
const OPEN: &str = "Open in browser";
const OPEN_MYSELF: &str = "Accept (I will open it myself)";
const SKIP: &str = "Skip";
const YES: &str = "Yes";
const NO: &str = "No";
/// Invalid entries of one field before the dialog counts as cancelled.
const MAX_TRIES: usize = 3;

/// The `mcp_elicitation` event.
fn elicitation_event(request: &ElicitationRequest) -> Value {
    let mut event = json!({
        "type": "mcp_elicitation",
        "server": request.server,
        "message": request.message,
        "mode": request.mode.as_str(),
    });
    if let Some(url) = &request.url {
        event["url"] = json!(url);
    }
    if let Some(id) = &request.elicitation_id {
        event["elicitationId"] = json!(id);
    }
    if let Some(schema) = &request.requested_schema {
        event["requestedSchema"] = schema.clone();
    }
    event
}

/// The `mcp_elicitation_result` event.
fn result_event(request: &ElicitationRequest, response: &ElicitationResponse) -> Value {
    let mut event = json!({
        "type": "mcp_elicitation_result",
        "server": request.server,
        "action": response.action.as_str(),
        "mode": request.mode.as_str(),
    });
    if let Some(id) = &request.elicitation_id {
        event["elicitationId"] = json!(id);
    }
    if let Some(content) = &response.content {
        event["content"] = Value::Object(content.clone());
    }
    event
}

impl AgentSession {
    /// How the session's MCP servers ask the user, for [`pi_mcp::McpManagerOptions::elicitation`].
    pub(crate) fn mcp_elicitation(&self) -> McpElicitation {
        let weak = self.weak.clone();
        let mut elicitation = McpElicitation::new(Arc::new(move |request, cancel| {
            let session = weak.upgrade();
            async move {
                match session {
                    Some(session) => session.elicit_mcp(request, cancel).await,
                    None => ElicitationResponse::decline(),
                }
            }
            .boxed()
        }));
        let weak = self.weak.clone();
        elicitation.on_complete = Some(Arc::new(move |server, _| {
            if let Some(session) = weak.upgrade() {
                session
                    .notify(&format!("MCP server \"{server}\" completed the request you opened."), NotifyLevel::Info);
            }
        }));
        elicitation
    }

    async fn elicit_mcp(&self, request: ElicitationRequest, cancel: CancellationToken) -> ElicitationResponse {
        let runner = self.extension_runner();
        let handlers = |name: &str| runner.clone().filter(|runner| runner.has_handlers(name));
        if let Some(runner) = handlers("mcp_elicitation") {
            let answer = runner.call_json("emitMcpElicitation", vec![elicitation_event(&request)]).await;
            if let Some(response) = answer.ok().as_ref().and_then(ElicitationResponse::from_json) {
                return response;
            }
        }
        let ui = self.extension_bindings().filter(|bindings| bindings.mode.has_ui()).map(|bindings| bindings.ui);
        let Some(ui) = ui else { return ElicitationResponse::decline() };
        let response = ask(ui.as_ref(), &request, &cancel).await;
        let Some(runner) = handlers("mcp_elicitation_result") else { return response };
        let replaced = runner.call_json("emitMcpElicitationResult", vec![result_event(&request, &response)]).await;
        replaced.ok().as_ref().and_then(ElicitationResponse::from_json).unwrap_or(response)
    }
}

/// Ask the user. A dismissed dialog (Esc) cancels.
async fn ask(ui: &dyn ExtensionUi, request: &ElicitationRequest, cancel: &CancellationToken) -> ElicitationResponse {
    let options = || DialogOptions { cancel: Some(cancel.clone()), ..DialogOptions::default() };
    match request.mode {
        ElicitationMode::Url => ask_url(ui, request, options()).await,
        ElicitationMode::Form => ask_form(ui, request, &options).await.unwrap_or_else(ElicitationResponse::cancel),
    }
}

async fn ask_url(ui: &dyn ExtensionUi, request: &ElicitationRequest, options: DialogOptions) -> ElicitationResponse {
    let url = request.url.clone().unwrap_or_default();
    let host = url::Url::parse(&url).ok().and_then(|url| url.host_str().map(str::to_string)).unwrap_or_default();
    let mut title = format!(
        "MCP server \"{}\" asks you to open a URL\n\n{}\n\n{url}\nDomain: {host}",
        request.server, request.message
    );
    if host.split('.').any(|label| label.starts_with("xn--")) {
        title.push_str("\nWarning: the domain uses Punycode and can look like a different domain.");
    }
    let choices = [OPEN, OPEN_MYSELF, DECLINE].map(String::from);
    match ui.select(&title, &choices, options).await.as_deref() {
        Some(OPEN) => {
            pi_ai::auth::oauth::open_browser(&url);
            ElicitationResponse::accept(None)
        }
        Some(OPEN_MYSELF) => ElicitationResponse::accept(None),
        Some(_) => ElicitationResponse::decline(),
        None => ElicitationResponse::cancel(),
    }
}

/// `None` when the user dismissed a dialog.
async fn ask_form(
    ui: &dyn ExtensionUi,
    request: &ElicitationRequest,
    options: &(dyn Fn() -> DialogOptions + Sync),
) -> Option<ElicitationResponse> {
    let schema = request.requested_schema.clone().unwrap_or_default();
    let properties = schema["properties"].as_object().cloned().unwrap_or_default();
    let required: Vec<&str> = schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let title = format!("MCP server \"{}\" asks for information\n\n{}", request.server, request.message);
    if !properties.is_empty() {
        let choices = [RESPOND, DECLINE].map(String::from);
        if ui.select(&title, &choices, options()).await? != RESPOND {
            return Some(ElicitationResponse::decline());
        }
    }
    let mut content = Map::new();
    for (name, property) in &properties {
        if let Some(value) = ask_field(ui, name, property, required.contains(&name.as_str()), options).await? {
            content.insert(name.clone(), value);
        }
    }
    let summary: Vec<String> = content
        .iter()
        .map(|(name, value)| match value {
            Value::String(text) => format!("{name}: {text}"),
            value => format!("{name}: {value}"),
        })
        .collect();
    let confirm = if summary.is_empty() { title } else { format!("{title}\n\n{}", summary.join("\n")) };
    let choices = [ACCEPT, DECLINE].map(String::from);
    Some(match ui.select(&confirm, &choices, options()).await?.as_str() {
        ACCEPT => ElicitationResponse::accept(Some(content)),
        _ => ElicitationResponse::decline(),
    })
}

/// Labels of enum values: `oneOf`/`anyOf` titles, legacy `enumNames`, or the values.
fn enum_labels(property: &Value, values: &[String]) -> Vec<String> {
    let titled = property.get("oneOf").or_else(|| property.get("anyOf")).and_then(Value::as_array);
    let names = property.get("enumNames").and_then(Value::as_array);
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let title = titled
                .and_then(|options| options.iter().find(|option| option["const"] == json!(value)))
                .and_then(|option| option["title"].as_str())
                .or_else(|| names.and_then(|names| names.get(index)).and_then(Value::as_str));
            title.map_or_else(|| value.clone(), |title| format!("{title} ({value})"))
        })
        .collect()
}

/// One field: `Some(None)` skips an optional field, `None` cancels.
async fn ask_field(
    ui: &dyn ExtensionUi,
    name: &str,
    property: &Value,
    required: bool,
    options: &(dyn Fn() -> DialogOptions + Sync),
) -> Option<Option<Value>> {
    let mut title = property["title"].as_str().unwrap_or(name).to_string();
    if !required {
        title.push_str(" (optional)");
    }
    if let Some(description) = property["description"].as_str() {
        title = format!("{title}\n{description}");
    }
    let kind = property["type"].as_str().unwrap_or("string");
    let choices = match kind {
        "boolean" => Some((vec![YES.to_string(), NO.to_string()], vec![json!(true), json!(false)])),
        "string" => enum_values(property)
            .map(|values| (enum_labels(property, &values), values.into_iter().map(Value::from).collect())),
        _ => None,
    };
    if let Some((mut labels, mut values)) = choices {
        // The default value comes first, so the dialog selects it.
        if let Some(index) = property.get("default").and_then(|default| values.iter().position(|v| v == default)) {
            labels[..=index].rotate_right(1);
            values[..=index].rotate_right(1);
        }
        if !required {
            labels.push(SKIP.to_string());
        }
        let picked = ui.select(&title, &labels, options()).await?;
        return Some(labels.iter().position(|label| *label == picked).and_then(|index| values.get(index).cloned()));
    }
    let allowed = property.get("items").and_then(enum_values);
    let hint = match (kind, &allowed) {
        ("array", Some(values)) => format!("Comma-separated: {}", values.join(", ")),
        ("array", None) => "Comma-separated".to_string(),
        ("number", _) => "A number".to_string(),
        ("integer", _) => "A whole number".to_string(),
        _ => property["format"].as_str().unwrap_or_default().to_string(),
    };
    let default = property.get("default").map(|value| match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
        value => value.to_string(),
    });
    let mut prompt = title.clone();
    for _ in 0..MAX_TRIES {
        let dialog = DialogOptions { prefill: default.clone(), ..options() };
        let text = ui.input(&prompt, Some(&hint).filter(|hint| !hint.is_empty()).map(String::as_str), dialog).await?;
        let text = text.trim();
        if text.is_empty() {
            if !required {
                return Some(None);
            }
            prompt = format!("{title}\nA value is required.");
            continue;
        }
        match parse_field(kind, text, allowed.as_deref()) {
            Some(value) => return Some(Some(value)),
            None => prompt = format!("{title}\nInvalid value: {hint}."),
        }
    }
    None
}

fn parse_field(kind: &str, text: &str, allowed: Option<&[String]>) -> Option<Value> {
    match kind {
        "number" => text.parse::<f64>().ok().filter(|number| number.is_finite()).map(Value::from),
        "integer" => text.parse::<i64>().ok().map(Value::from),
        "array" => {
            let items: Vec<String> =
                text.split(',').map(str::trim).filter(|item| !item.is_empty()).map(str::to_string).collect();
            let valid = allowed.is_none_or(|allowed| items.iter().all(|item| allowed.contains(item)));
            valid.then(|| json!(items))
        }
        _ => Some(json!(text)),
    }
}
