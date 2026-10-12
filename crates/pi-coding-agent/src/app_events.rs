//! Session events: agent events plus auto retry progress, and their JSON wire form
//! (upstream `AgentSessionEvent` and `modes/json-event.ts`).

use pi_agent::AgentEvent;
use pi_ai::{ContentBlock, Message};
use serde_json::{Value, json};

/// One event of a coding session, borrowed for the duration of a listener call.
#[derive(Clone, Copy, Debug)]
pub enum SessionEvent<'a> {
    /// Any agent event other than `agent_end`.
    Agent(&'a AgentEvent),
    /// The end of an agent run; `will_retry` is set when auto retry continues the run.
    AgentEnd {
        messages: &'a [Message],
        will_retry: bool,
    },
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: &'a str,
    },
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        final_error: Option<&'a str>,
    },
}

impl SessionEvent<'_> {
    /// The JSON line for `--mode json`. `message_update` drops the cumulative `partial`
    /// snapshot and keeps the delta, the usage, and for `toolcall_start` the call id and name.
    pub fn to_json(&self) -> Value {
        match self {
            SessionEvent::Agent(AgentEvent::MessageUpdate { message, assistant_message_event }) => {
                let usage = match message {
                    Message::Assistant(assistant) => serde_json::to_value(assistant.usage).unwrap_or(Value::Null),
                    _ => Value::Null,
                };
                json!({
                    "type": "message_update",
                    "usage": usage,
                    "assistantMessageEvent": delta_event(assistant_message_event),
                })
            }
            SessionEvent::Agent(AgentEvent::AgentEnd { messages }) => agent_end(messages, false),
            SessionEvent::Agent(event) => serde_json::to_value(event).unwrap_or(Value::Null),
            SessionEvent::AgentEnd { messages, will_retry } => agent_end(messages, *will_retry),
            SessionEvent::AutoRetryStart { attempt, max_attempts, delay_ms, error_message } => json!({
                "type": "auto_retry_start",
                "attempt": attempt,
                "maxAttempts": max_attempts,
                "delayMs": delay_ms,
                "errorMessage": error_message,
            }),
            SessionEvent::AutoRetryEnd { success, attempt, final_error } => {
                let mut event = json!({"type": "auto_retry_end", "success": success, "attempt": attempt});
                if let Some(error) = final_error {
                    event["finalError"] = json!(error);
                }
                event
            }
        }
    }
}

fn agent_end(messages: &[Message], will_retry: bool) -> Value {
    json!({"type": "agent_end", "messages": messages, "willRetry": will_retry})
}

fn delta_event(event: &pi_ai::AssistantMessageEvent) -> Value {
    let mut value = serde_json::to_value(event).unwrap_or(Value::Null);
    if let Some(fields) = value.as_object_mut() {
        fields.remove("partial");
        if let pi_ai::AssistantMessageEvent::ToolcallStart { content_index, partial } = event
            && let Some(ContentBlock::ToolCall(call)) = partial.content.get(*content_index)
        {
            fields.insert("id".into(), json!(call.id));
            fields.insert("toolName".into(), json!(call.name));
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use pi_ai::{AssistantMessage, AssistantMessageEvent, Model, ToolCall};

    use super::*;

    #[test]
    fn message_updates_carry_deltas_and_usage_only() {
        let mut partial = AssistantMessage::empty(&Model::default());
        partial.content.push(ContentBlock::text("Hel"));
        partial.usage.output = 3;
        let event = AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "lo".into(),
                partial: partial.clone(),
            },
        };
        let value = SessionEvent::Agent(&event).to_json();
        assert_eq!(value["type"], "message_update");
        assert_eq!(value["usage"]["output"], 3);
        assert_eq!(value["assistantMessageEvent"], json!({"type": "text_delta", "contentIndex": 0, "delta": "lo"}));
        assert!(value.get("message").is_none());

        partial.content =
            vec![ContentBlock::ToolCall(ToolCall { id: "c1".into(), name: "bash".into(), ..Default::default() })];
        let start = AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::ToolcallStart { content_index: 0, partial },
        };
        let value = SessionEvent::Agent(&start).to_json();
        assert_eq!(
            value["assistantMessageEvent"],
            json!({"type": "toolcall_start", "contentIndex": 0, "id": "c1", "toolName": "bash"})
        );
    }

    #[test]
    fn retry_events_use_upstream_field_names() {
        let start = SessionEvent::AutoRetryStart { attempt: 1, max_attempts: 3, delay_ms: 2000, error_message: "503" };
        assert_eq!(
            start.to_json(),
            json!({"type": "auto_retry_start", "attempt": 1, "maxAttempts": 3, "delayMs": 2000, "errorMessage": "503"})
        );
        let end = SessionEvent::AutoRetryEnd { success: true, attempt: 2, final_error: None };
        assert_eq!(end.to_json(), json!({"type": "auto_retry_end", "success": true, "attempt": 2}));
        let agent_end = SessionEvent::AgentEnd { messages: &[], will_retry: true };
        assert_eq!(agent_end.to_json(), json!({"type": "agent_end", "messages": [], "willRetry": true}));
    }
}
