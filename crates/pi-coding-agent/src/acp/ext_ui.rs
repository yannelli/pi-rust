//! Extension UI requests over ACP (upstream `handleExtensionUiRequest`): `select` and
//! `confirm` become permission requests, `notify` becomes a message chunk tagged with its
//! level, and the other dialogs are cancelled with a visible note.

use agent_client_protocol_schema::{
    ContentBlock, ContentChunk, PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionUpdate, TextContent, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use serde_json::{Map, Value, json};

use super::session::{AcpSession, meta_of};

const CHOICE_PREFIX: &str = "choice-";
const RAW_INPUT_KEYS: [&str; 5] = ["title", "message", "options", "placeholder", "prefill"];

fn string_prop<'a>(event: &'a Value, key: &str) -> Option<&'a str> {
    event.get(key).and_then(Value::as_str)
}

/// The option index of a `choice-N` id, rejecting anything but a canonical number.
pub fn option_index(option_id: &str) -> Option<usize> {
    let raw = option_id.strip_prefix(CHOICE_PREFIX).filter(|raw| !raw.is_empty())?;
    let index = raw.parse::<usize>().ok()?;
    (index.to_string() == raw).then_some(index)
}

/// The pending tool call a permission request shows for an extension dialog.
pub fn extension_ui_tool_call(id: &str, event: &Value) -> ToolCallUpdate {
    let method = string_prop(event, "method").unwrap_or("ui");
    let title = string_prop(event, "title").map(String::from).unwrap_or_else(|| format!("Pi {method}"));
    let mut raw_input = Map::new();
    raw_input.insert("method".into(), json!(method));
    for key in RAW_INPUT_KEYS {
        if let Some(value) = event.get(key) {
            raw_input.insert(key.into(), value.clone());
        }
    }
    let fields = ToolCallUpdateFields::new()
        .title(title)
        .kind(ToolKind::Other)
        .status(ToolCallStatus::Pending)
        .raw_input(Value::Object(raw_input));
    ToolCallUpdate::new(format!("pi-ui-{id}"), fields)
}

fn confirm_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new("yes", "Yes", PermissionOptionKind::AllowOnce),
        PermissionOption::new("no", "No", PermissionOptionKind::RejectOnce),
    ]
}

fn selected_option(response: &RequestPermissionResponse) -> Option<String> {
    match &response.outcome {
        RequestPermissionOutcome::Selected(selected) => Some(selected.option_id.to_string()),
        _ => None,
    }
}

impl AcpSession {
    async fn respond_ui(&self, response: Value) {
        if let Err(error) = self.backend.send_extension_ui_response(response).await {
            eprintln!("Warning: could not answer an extension dialog: {error:#}");
        }
    }

    pub(super) async fn handle_extension_ui_request(&self, event: Value) {
        let Some(id) = string_prop(&event, "id").map(String::from) else { return };
        match string_prop(&event, "method").unwrap_or_default() {
            "select" => self.extension_select(&event, &id).await,
            "confirm" => self.extension_confirm(&event, &id).await,
            method @ ("input" | "editor") => {
                let text = format!("Pi {method} UI request is not supported in ACP yet; cancelling it.");
                self.emit(SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
                    text,
                )))));
                self.respond_ui(json!({"id": id, "cancelled": true})).await;
            }
            "notify" => {
                let message = string_prop(&event, "message").unwrap_or("Pi notification");
                let level = string_prop(&event, "notifyType").unwrap_or("info");
                let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(message)))
                    .meta(meta_of(json!({"piAcp": {"notify": {"level": level}}})));
                self.emit(SessionUpdate::AgentMessageChunk(chunk));
                self.respond_ui(json!({"id": id, "cancelled": true})).await;
            }
            _ => self.respond_ui(json!({"id": id, "cancelled": true})).await,
        }
    }

    async fn extension_select(&self, event: &Value, id: &str) {
        let options: Vec<String> = event
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options.iter().map(|option| option.as_str().map_or_else(|| option.to_string(), String::from)).collect()
            })
            .unwrap_or_default();
        if options.is_empty() {
            return self.respond_ui(json!({"id": id, "cancelled": true})).await;
        }
        let permission_options = options
            .iter()
            .enumerate()
            .map(|(index, name)| {
                PermissionOption::new(format!("{CHOICE_PREFIX}{index}"), name.clone(), PermissionOptionKind::AllowOnce)
            })
            .collect();
        let Some(response) = self.request_extension_permission(id, event, permission_options).await else { return };
        let value = selected_option(&response).and_then(|option| option_index(&option)).and_then(|i| options.get(i));
        let reply = match value {
            Some(value) => json!({"id": id, "value": value}),
            None => json!({"id": id, "cancelled": true}),
        };
        self.respond_ui(reply).await;
    }

    async fn extension_confirm(&self, event: &Value, id: &str) {
        let Some(response) = self.request_extension_permission(id, event, confirm_options()).await else { return };
        let reply = match selected_option(&response) {
            None => json!({"id": id, "cancelled": true}),
            Some(option) => json!({"id": id, "confirmed": option == "yes"}),
        };
        self.respond_ui(reply).await;
    }

    /// Ask the client; a failed request cancels the dialog and yields `None`.
    async fn request_extension_permission(
        &self,
        id: &str,
        event: &Value,
        options: Vec<PermissionOption>,
    ) -> Option<RequestPermissionResponse> {
        let request =
            RequestPermissionRequest::new(self.session_id.clone(), extension_ui_tool_call(id, event), options);
        match self.client().request_permission(request).await {
            Ok(response) => Some(response),
            Err(_) => {
                self.respond_ui(json!({"id": id, "cancelled": true})).await;
                None
            }
        }
    }
}
