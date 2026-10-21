//! Automatic compaction decisions: the threshold check between turns and after a run, and
//! overflow or early `length` recovery with `context_edit` omissions. Port of upstream
//! `AgentSession._checkCompaction` and `_omitRecoveryAttempt`; the app runs the compaction.

use pi_ai::{AssistantMessage, Message, Model, StopReason, is_context_overflow, is_recoverable_length};

use super::{
    CompactionSettings, calculate_context_tokens, estimate_context_tokens, estimate_projected_context_tokens,
    should_compact,
};
use crate::session::{EntryData, SessionManager, parse_iso};

/// Whether the projected context of the current branch crosses `model`'s threshold. This is
/// the check before each assistant response, including between turns of one run.
pub fn exceeds_compaction_threshold(session: &SessionManager, model: &Model, settings: &CompactionSettings) -> bool {
    if model.context_window == 0 {
        return false;
    }
    let projection = session.build_session_projection();
    let tokens = estimate_projected_context_tokens(&projection, &session.get_branch(None)).tokens;
    should_compact(tokens, model.context_window, settings)
}

/// What to do after an assistant response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactionCheck {
    Nothing,
    /// Context crossed the threshold: compact, then continue only for queued messages.
    Threshold,
    /// Overflow or recoverable truncation. With `will_retry`, first omit the response (and
    /// its tool results) with [`omit_recovery_attempt`], compact, then retry the turn once.
    Overflow {
        will_retry: bool,
    },
    /// Recovery already ran once in this run: report the error and stop.
    RecoveryExhausted {
        error_message: String,
    },
}

/// Inputs for [`check_compaction`].
pub struct CompactionCheckInput<'a> {
    pub assistant: &'a AssistantMessage,
    /// The session entry holding `assistant`, if it was persisted.
    pub assistant_entry_id: Option<&'a str>,
    /// The model that produced the response, or `None` when it differs from the current one.
    pub message_model: Option<&'a Model>,
    pub current_model: &'a Model,
    pub settings: &'a CompactionSettings,
    /// Ignore aborted responses (false for the check before a new prompt).
    pub skip_aborted: bool,
    /// Whether a recovery already ran in this run.
    pub recovery_attempted: bool,
    /// The agent's current messages, for estimates when the response has no usage.
    pub agent_messages: &'a [Message],
}

/// Decide whether a finished response needs automatic compaction.
pub fn check_compaction(session: &SessionManager, input: CompactionCheckInput) -> CompactionCheck {
    let assistant = input.assistant;
    if !input.settings.enabled || (input.skip_aborted && assistant.stop_reason == StopReason::Aborted) {
        return CompactionCheck::Nothing;
    }
    let same_model = input.message_model.is_some();
    let context_window = input.message_model.unwrap_or(input.current_model).context_window;
    let branch = session.get_branch(None);
    let compaction_time = branch
        .iter()
        .rev()
        .find(|entry| entry.compaction().is_some())
        .map(|entry| parse_iso(entry.timestamp()).unwrap_or(0));
    if compaction_time.is_some_and(|time| assistant.timestamp <= time) {
        return CompactionCheck::Nothing;
    }
    let projection = session.build_session_projection();
    let entry_id = input.assistant_entry_id;
    let projected = entry_id.is_none_or(|id| {
        projection.entries.iter().any(|entry| {
            entry.source.id() == id && entry.messages.iter().any(|message| matches!(message, Message::Assistant(_)))
        })
    });
    let index = entry_id.and_then(|id| branch.iter().position(|entry| entry.id() == id));
    let after = index.map_or(&[][..], |index| &branch[index + 1..]);
    let edited_after = after.iter().any(|entry| entry.kind() == "context_edit");
    let latest_edit = after.iter().rev().find_map(|entry| match entry.data() {
        EntryData::ContextEdit { target_id, replacement } if Some(target_id.as_str()) == entry_id => Some(replacement),
        _ => None,
    });
    let retained = entry_id.is_none()
        || (!after.iter().any(|entry| entry.compaction().is_some()) && latest_edit.is_none_or(Option::is_some));
    let explicit_overflow = assistant.stop_reason == StopReason::Error && is_context_overflow(assistant, None);
    let context_overflow = same_model
        && ((explicit_overflow && retained)
            || (projected && !edited_after && is_context_overflow(assistant, Some(context_window))));
    let recoverable_length =
        projected && input.message_model.is_some_and(|model| is_recoverable_length(assistant, model.max_tokens));
    if context_overflow || recoverable_length {
        let will_retry = assistant.stop_reason != StopReason::Stop;
        if !will_retry {
            return CompactionCheck::Overflow { will_retry: false };
        }
        if input.recovery_attempted {
            let error_message = if context_overflow {
                "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model."
            } else {
                "Truncated response recovery failed after one compact-and-retry attempt."
            };
            return CompactionCheck::RecoveryExhausted { error_message: error_message.into() };
        }
        return CompactionCheck::Overflow { will_retry: true };
    }
    let has_edits = projection.entries.iter().any(|entry| entry.source.kind() == "context_edit");
    let direct = calculate_context_tokens(&assistant.usage);
    let tokens = if has_edits {
        estimate_projected_context_tokens(&projection, &branch).tokens
    } else if assistant.stop_reason == StopReason::Error || direct == 0 {
        let estimate = estimate_context_tokens(input.agent_messages);
        let stale = estimate.last_usage_index.and_then(|index| match &input.agent_messages[index] {
            Message::Assistant(message) => compaction_time.map(|time| message.timestamp <= time),
            _ => None,
        });
        if stale == Some(true) {
            return CompactionCheck::Nothing;
        }
        estimate.tokens
    } else {
        direct
    };
    if should_compact(tokens, context_window, input.settings) {
        CompactionCheck::Threshold
    } else {
        CompactionCheck::Nothing
    }
}

/// The id of the newest entry on the branch that stores `message`.
pub fn find_message_entry_id(session: &SessionManager, message: &Message) -> Option<String> {
    session.get_branch(None).iter().rev().find(|entry| entry.message() == Some(message)).map(|e| e.id().to_string())
}

/// The ids of the entries after `assistant_entry_id`, and before the next assistant message,
/// that hold results of `assistant`'s tool calls.
pub fn find_tool_result_entry_ids(
    session: &SessionManager,
    assistant_entry_id: &str,
    assistant: &AssistantMessage,
) -> Vec<String> {
    let branch = session.get_branch(None);
    let Some(index) = branch.iter().position(|entry| entry.id() == assistant_entry_id) else { return Vec::new() };
    let calls: Vec<&str> = assistant.tool_calls().map(|call| call.id.as_str()).collect();
    branch[index + 1..]
        .iter()
        .take_while(|entry| !matches!(entry.message(), Some(Message::Assistant(_))))
        .filter(|entry| {
            matches!(entry.message(), Some(Message::ToolResult(result)) if calls.contains(&result.tool_call_id.as_str()))
        })
        .map(|entry| entry.id().to_string())
        .collect()
}

/// Omit a failed response and its tool results from future context with `context_edit`
/// entries. Returns the ids of the appended edits.
pub fn omit_recovery_attempt(session: &mut SessionManager, targets: &[&str]) -> anyhow::Result<Vec<String>> {
    targets.iter().map(|target| session.append_context_edit(target, None)).collect()
}
