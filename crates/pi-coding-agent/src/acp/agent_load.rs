//! `session/load` (upstream `PiAcpAgent.loadSession`): reopen the stored pi session and replay
//! its conversation as session updates.

use agent_client_protocol_schema::{
    ContentBlock, ContentChunk, LoadSessionRequest, SessionNotification, SessionUpdate, TextContent, ToolCall,
    ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use serde_json::{Value, json};

use super::agent::{AcpAgent, Handled};
use super::bash::{
    bash_command, bash_exit_code, bash_result_text, bash_terminal_content, bash_terminal_exit_meta,
    bash_terminal_info_meta, bash_terminal_output_meta, is_bash_tool,
};
use super::config::session_configuration;
use super::errors::{AcpResult, internal_failure, invalid_params};
use super::translate::{normalize_pi_assistant_text, normalize_pi_message_text, tool_kind, tool_result_to_text};
use crate::session::ids::uuid_v4;

fn text(text: String) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
}

/// The session updates that replay one stored message.
pub fn replay_message(message: &Value, cwd: &str) -> Vec<SessionUpdate> {
    match message["role"].as_str().unwrap_or_default() {
        "user" => {
            let content = normalize_pi_message_text(&message["content"]);
            (!content.is_empty()).then(|| SessionUpdate::UserMessageChunk(text(content))).into_iter().collect()
        }
        "assistant" => {
            let content = normalize_pi_assistant_text(&message["content"]);
            (!content.is_empty()).then(|| SessionUpdate::AgentMessageChunk(text(content))).into_iter().collect()
        }
        "toolResult" => replay_tool_result(message, cwd),
        _ => Vec::new(),
    }
}

fn replay_tool_result(message: &Value, cwd: &str) -> Vec<SessionUpdate> {
    let name = message["toolName"].as_str().unwrap_or("tool").to_string();
    let id = message["toolCallId"].as_str().map(String::from).unwrap_or_else(uuid_v4);
    let is_error = message["isError"].as_bool().unwrap_or(false);
    let status = if is_error { ToolCallStatus::Failed } else { ToolCallStatus::Completed };
    if is_bash_tool(&name) {
        let output = bash_result_text(message);
        let call = ToolCall::new(id.clone(), bash_command(message).unwrap_or_else(|| name.clone()))
            .kind(ToolKind::Execute)
            .status(ToolCallStatus::Completed)
            .content(bash_terminal_content(&id))
            .meta(bash_terminal_info_meta(&id, cwd));
        let mut meta = if output.is_empty() { Default::default() } else { bash_terminal_output_meta(&id, &output) };
        meta.extend(bash_terminal_exit_meta(&id, bash_exit_code(message, is_error)));
        let update = ToolCallUpdate::new(id, ToolCallUpdateFields::new().status(status)).meta(meta);
        return vec![SessionUpdate::ToolCall(call), SessionUpdate::ToolCallUpdate(update)];
    }
    let call = ToolCall::new(id.clone(), name.clone())
        .kind(tool_kind(&name))
        .status(ToolCallStatus::Completed)
        .raw_output(message.clone());
    let output = tool_result_to_text(message);
    let mut fields = ToolCallUpdateFields::new().status(status).raw_output(message.clone());
    if !output.is_empty() {
        fields = fields.content(vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(output)))]);
    }
    vec![SessionUpdate::ToolCall(call), SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id, fields))]
}

impl AcpAgent {
    pub async fn load_session(&self, params: LoadSessionRequest) -> AcpResult<Handled<Value>> {
        if !super::settings::node_is_absolute(&params.cwd) {
            return Err(invalid_params(format!("cwd must be an absolute path: {}", params.cwd.display())));
        }
        let session_id = params.session_id.0.to_string();
        let cwd = params.cwd.display().to_string();
        self.close(&session_id).await;
        self.set_last_cwd(&cwd);
        let Some((_, file)) = self.find_stored_session(&session_id) else {
            return Err(invalid_params(format!("Unknown sessionId: {session_id}")));
        };
        let session = self.restore_session(&session_id, Some(&cwd)).await?;
        let configuration = match session_configuration(session.backend.as_ref(), None, None).await {
            Ok(configuration) => configuration,
            Err(error) => {
                self.close(&session_id).await;
                return Err(internal_failure(&error));
            }
        };
        self.close_all_except(&session_id).await;
        self.store.upsert(&session_id, &cwd, &file);
        let data = session.backend.get_messages().await.map_err(|error| internal_failure(&error))?;
        for message in data["messages"].as_array().into_iter().flatten() {
            for update in replay_message(message, &cwd) {
                self.client().session_update(SessionNotification::new(session_id.clone(), update));
            }
        }
        let response = json!({
            "configOptions": configuration.config_options,
            "models": configuration.models,
            "modes": configuration.modes,
            "_meta": {"piAcp": {"startupInfo": null}},
        });
        let (agent, cwd) = (self.this(), params.cwd.clone());
        let after = async move { agent.advertise_commands(&session, &cwd).await };
        Ok(Handled { response, after: Some(Box::pin(after)) })
    }
}
