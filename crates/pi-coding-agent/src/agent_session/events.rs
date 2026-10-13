//! Session events (upstream `AgentSessionEvent`) and their JSON wire form for JSON and RPC
//! modes (upstream `modes/json-event.ts`, `docs/json.md`).

use pi_agent::AgentEvent;
use pi_ai::{AssistantMessageEvent, ContentBlock, Message, ThinkingLevel};
use serde_json::{Map, Value, json};

use crate::compaction::CompactionResult;

/// Why a compaction ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

impl CompactionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Threshold => "threshold",
            Self::Overflow => "overflow",
        }
    }
}

/// Which summarization a summarization retry belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SummarizationSource {
    BranchSummary,
    Compaction(CompactionReason),
}

/// One event of an [`super::AgentSession`], borrowed for the duration of a listener call.
#[derive(Clone, Debug)]
pub enum AgentSessionEvent<'a> {
    /// Any agent event other than `agent_end`.
    Agent(&'a AgentEvent),
    /// The end of one low-level agent run; `will_retry` is set when auto retry continues it.
    AgentEnd {
        messages: &'a [Message],
        will_retry: bool,
    },
    /// No automatic work (retry, recovery, queued messages) remains for the run.
    AgentSettled {
        aborted: bool,
    },
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    CompactionStart {
        reason: CompactionReason,
    },
    CompactionEnd {
        reason: CompactionReason,
        result: Option<&'a CompactionResult>,
        aborted: bool,
        will_retry: bool,
        error_message: Option<String>,
    },
    /// A session entry appended outside the message flow, as stored.
    EntryAppended {
        entry: Value,
    },
    SessionInfoChanged {
        name: Option<String>,
    },
    ThinkingLevelChanged {
        level: ThinkingLevel,
    },
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        final_error: Option<String>,
    },
    SummarizationRetryScheduled {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    SummarizationRetryAttemptStart {
        source: SummarizationSource,
    },
    SummarizationRetryFinished,
    /// One output chunk of a direct bash command (RPC `bash`, `!cmd`).
    BashExecutionUpdate {
        id: Option<String>,
        delta: String,
    },
}

impl AgentSessionEvent<'_> {
    /// The event's `type` tag.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Agent(event) => agent_event_kind(event),
            Self::AgentEnd { .. } => "agent_end",
            Self::AgentSettled { .. } => "agent_settled",
            Self::QueueUpdate { .. } => "queue_update",
            Self::CompactionStart { .. } => "compaction_start",
            Self::CompactionEnd { .. } => "compaction_end",
            Self::EntryAppended { .. } => "entry_appended",
            Self::SessionInfoChanged { .. } => "session_info_changed",
            Self::ThinkingLevelChanged { .. } => "thinking_level_changed",
            Self::AutoRetryStart { .. } => "auto_retry_start",
            Self::AutoRetryEnd { .. } => "auto_retry_end",
            Self::SummarizationRetryScheduled { .. } => "summarization_retry_scheduled",
            Self::SummarizationRetryAttemptStart { .. } => "summarization_retry_attempt_start",
            Self::SummarizationRetryFinished => "summarization_retry_finished",
            Self::BashExecutionUpdate { .. } => "bash_execution_update",
        }
    }

    /// The JSON record for `--mode json` and RPC. `message_update` drops the cumulative
    /// `partial` snapshot and keeps the delta, the usage, and for `toolcall_start` the call id and name.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Agent(AgentEvent::MessageUpdate { message, assistant_message_event }) => {
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
            Self::Agent(AgentEvent::AgentEnd { messages }) => agent_end(messages, false),
            Self::Agent(event) => serde_json::to_value(event).unwrap_or(Value::Null),
            Self::AgentEnd { messages, will_retry } => agent_end(messages, *will_retry),
            Self::AgentSettled { aborted } => json!({"type": "agent_settled", "aborted": aborted}),
            Self::QueueUpdate { steering, follow_up } => {
                json!({"type": "queue_update", "steering": steering, "followUp": follow_up})
            }
            Self::CompactionStart { reason } => json!({"type": "compaction_start", "reason": reason.as_str()}),
            Self::CompactionEnd { reason, result, aborted, will_retry, error_message } => {
                let mut event = Map::new();
                event.insert("type".into(), json!("compaction_end"));
                event.insert("reason".into(), json!(reason.as_str()));
                if let Some(result) = result {
                    event.insert("result".into(), compaction_result_json(result));
                }
                event.insert("aborted".into(), json!(aborted));
                event.insert("willRetry".into(), json!(will_retry));
                if let Some(message) = error_message {
                    event.insert("errorMessage".into(), json!(message));
                }
                Value::Object(event)
            }
            Self::EntryAppended { entry } => json!({"type": "entry_appended", "entry": entry}),
            Self::SessionInfoChanged { name } => match name {
                Some(name) => json!({"type": "session_info_changed", "name": name}),
                None => json!({"type": "session_info_changed"}),
            },
            Self::ThinkingLevelChanged { level } => json!({"type": "thinking_level_changed", "level": level.as_str()}),
            Self::AutoRetryStart { attempt, max_attempts, delay_ms, error_message } => json!({
                "type": "auto_retry_start",
                "attempt": attempt,
                "maxAttempts": max_attempts,
                "delayMs": delay_ms,
                "errorMessage": error_message,
            }),
            Self::AutoRetryEnd { success, attempt, final_error } => {
                let mut event = json!({"type": "auto_retry_end", "success": success, "attempt": attempt});
                if let Some(error) = final_error {
                    event["finalError"] = json!(error);
                }
                event
            }
            Self::SummarizationRetryScheduled { attempt, max_attempts, delay_ms, error_message } => json!({
                "type": "summarization_retry_scheduled",
                "attempt": attempt,
                "maxAttempts": max_attempts,
                "delayMs": delay_ms,
                "errorMessage": error_message,
            }),
            Self::SummarizationRetryAttemptStart { source } => match source {
                SummarizationSource::BranchSummary => {
                    json!({"type": "summarization_retry_attempt_start", "source": "branchSummary"})
                }
                SummarizationSource::Compaction(reason) => json!({
                    "type": "summarization_retry_attempt_start",
                    "source": "compaction",
                    "reason": reason.as_str(),
                }),
            },
            Self::SummarizationRetryFinished => json!({"type": "summarization_retry_finished"}),
            Self::BashExecutionUpdate { id, delta } => match id {
                Some(id) => json!({"type": "bash_execution_update", "id": id, "delta": delta}),
                None => json!({"type": "bash_execution_update", "delta": delta}),
            },
        }
    }
}

fn agent_event_kind(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::AgentStart => "agent_start",
        AgentEvent::AgentEnd { .. } => "agent_end",
        AgentEvent::TurnStart => "turn_start",
        AgentEvent::TurnEnd { .. } => "turn_end",
        AgentEvent::MessageStart { .. } => "message_start",
        AgentEvent::MessageUpdate { .. } => "message_update",
        AgentEvent::MessageEnd { .. } => "message_end",
        AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
        AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
        AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
    }
}

/// A compaction result in upstream field order; `estimatedTokensAfter` and `details` are
/// left out when unknown.
pub fn compaction_result_json(result: &CompactionResult) -> Value {
    let mut object = Map::new();
    object.insert("summary".into(), json!(result.summary));
    object.insert("firstKeptEntryId".into(), json!(result.first_kept_entry_id));
    object.insert("tokensBefore".into(), json!(result.tokens_before));
    if let Some(after) = result.estimated_tokens_after {
        object.insert("estimatedTokensAfter".into(), json!(after));
    }
    object.insert("usage".into(), serde_json::to_value(result.usage).unwrap_or(Value::Null));
    if let Some(details) = &result.details {
        object.insert("details".into(), details.clone());
    }
    Value::Object(object)
}

fn agent_end(messages: &[Message], will_retry: bool) -> Value {
    json!({"type": "agent_end", "messages": messages, "willRetry": will_retry})
}

fn delta_event(event: &AssistantMessageEvent) -> Value {
    let mut value = serde_json::to_value(event).unwrap_or(Value::Null);
    if let Some(fields) = value.as_object_mut() {
        fields.remove("partial");
        if let AssistantMessageEvent::ToolcallStart { content_index, partial } = event
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
    use pi_ai::{AssistantMessage, Model, ToolCall};

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
        let value = AgentSessionEvent::Agent(&event).to_json();
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
        let value = AgentSessionEvent::Agent(&start).to_json();
        assert_eq!(
            value["assistantMessageEvent"],
            json!({"type": "toolcall_start", "contentIndex": 0, "id": "c1", "toolName": "bash"})
        );
    }

    #[test]
    fn session_events_use_upstream_shapes() {
        let start = AgentSessionEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
            error_message: "503".into(),
        };
        assert_eq!(
            start.to_json(),
            json!({"type": "auto_retry_start", "attempt": 1, "maxAttempts": 3, "delayMs": 2000, "errorMessage": "503"})
        );
        let end = AgentSessionEvent::AutoRetryEnd { success: true, attempt: 2, final_error: None };
        assert_eq!(end.to_json(), json!({"type": "auto_retry_end", "success": true, "attempt": 2}));
        let agent_end = AgentSessionEvent::AgentEnd { messages: &[], will_retry: true };
        assert_eq!(agent_end.to_json(), json!({"type": "agent_end", "messages": [], "willRetry": true}));
        let failed = AgentSessionEvent::CompactionEnd {
            reason: CompactionReason::Overflow,
            result: None,
            aborted: false,
            will_retry: false,
            error_message: Some("boom".into()),
        };
        assert_eq!(
            failed.to_json(),
            json!({"type": "compaction_end", "reason": "overflow", "aborted": false, "willRetry": false, "errorMessage": "boom"})
        );
        let cleared = AgentSessionEvent::SessionInfoChanged { name: None };
        assert_eq!(cleared.to_json(), json!({"type": "session_info_changed"}));
        let attempt = AgentSessionEvent::SummarizationRetryAttemptStart {
            source: SummarizationSource::Compaction(CompactionReason::Threshold),
        };
        assert_eq!(
            attempt.to_json(),
            json!({"type": "summarization_retry_attempt_start", "source": "compaction", "reason": "threshold"})
        );
        let bash = AgentSessionEvent::BashExecutionUpdate { id: Some("req-1".into()), delta: "x\n".into() };
        assert_eq!(bash.to_json(), json!({"type": "bash_execution_update", "id": "req-1", "delta": "x\n"}));
        assert_eq!(AgentSessionEvent::SummarizationRetryFinished.kind(), "summarization_retry_finished");
    }
}
