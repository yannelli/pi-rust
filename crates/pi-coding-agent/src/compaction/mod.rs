//! Context compaction and branch summaries: summarize older turns into a checkpoint so long
//! sessions fit the model's context window. Port of upstream `core/compaction/*`; see
//! `docs/compaction.md`.

pub mod branch;
pub mod cut_point;
pub mod prepare;
pub mod recovery;
pub mod summarize;
pub mod utils;

use pi_ai::{ContentBlock, Message, StopReason, SystemMessage, Usage, UserContent};
use serde_json::Value;

use crate::messages::{BashExecutionMessage, CustomRole};
use crate::session::ids::js_len;
use crate::session::{Entry, SessionProjection};

pub use self::branch::{
    BranchPreparation, BranchSummaryOptions, BranchSummaryResult, BranchSummarySettings, ProvidedSummary,
    TreeNavigation, TreeNavigationRequest, collect_entries_for_branch_summary, generate_branch_summary, navigate_tree,
    prepare_branch_entries,
};
pub use self::cut_point::{CutPointResult, find_cut_point, find_projected_cut_point, find_turn_start_index};
pub use self::prepare::{CompactionDetails, CompactionPreparation, prepare_compaction, prepare_compaction_entries};
pub use self::recovery::{
    CompactionCheck, CompactionCheckInput, check_compaction, exceeds_compaction_threshold, find_message_entry_id,
    omit_recovery_attempt,
};
pub use self::summarize::{
    CompactionResult, RetryEventFn, SummarizationOptions, compact, compact_session, complete_summarization,
    generate_summary, generate_summary_with_usage, summarization_failure, summarize_compaction,
};
pub use self::utils::{
    FileOperations, SUMMARIZATION_SYSTEM_PROMPT, compute_file_lists, extract_file_ops_from_message,
    format_file_operations, serialize_conversation,
};

/// The plan `prepare_compaction` returns, under its older name.
pub type CompactionPlan = CompactionPreparation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionSettings {
    pub enabled: bool,
    /// Tokens kept free for the next response.
    pub reserve_tokens: u64,
    /// Approximate size of recent history kept verbatim.
    pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self { enabled: true, reserve_tokens: 16_384, keep_recent_tokens: 20_000 }
    }
}

/// Context tokens from reported usage: `total_tokens`, else the sum of its parts.
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

/// Usage of an assistant message that finished normally with nonzero usage.
pub fn assistant_usage(message: &Message) -> Option<&Usage> {
    match message {
        Message::Assistant(assistant)
            if !matches!(assistant.stop_reason, StopReason::Aborted | StopReason::Error)
                && calculate_context_tokens(&assistant.usage) > 0 =>
        {
            Some(&assistant.usage)
        }
        _ => None,
    }
}

/// The last valid assistant usage among `entries`.
pub fn last_assistant_usage<'a>(entries: impl DoubleEndedIterator<Item = &'a Entry>) -> Option<&'a Usage> {
    entries.rev().find_map(|entry| entry.message().and_then(assistant_usage))
}

const ESTIMATED_IMAGE_CHARS: usize = 4800;

fn blocks_chars(blocks: &[ContentBlock]) -> usize {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text, .. } => js_len(text),
            ContentBlock::Image { .. } => ESTIMATED_IMAGE_CHARS,
            _ => 0,
        })
        .sum()
}

fn content_chars(content: &UserContent) -> usize {
    match content {
        UserContent::Text(text) => js_len(text),
        UserContent::Blocks(blocks) => blocks_chars(blocks),
    }
}

fn system_chars(system: &SystemMessage) -> usize {
    let sections: usize = system.sections.iter().flatten().filter_map(|(_, value)| value.as_str()).map(js_len).sum();
    let tools =
        system.tools_added.as_ref().map_or(0, |tools| js_len(&serde_json::to_string(tools).unwrap_or_default()));
    content_chars(&system.content) + sections + tools
}

fn custom_chars(custom: &pi_ai::CustomMessage) -> usize {
    match custom.role.as_str() {
        "custom" => match custom.fields.get("content") {
            Some(Value::String(text)) => js_len(text),
            Some(value) => serde_json::from_value::<Vec<ContentBlock>>(value.clone()).map_or(0, |b| blocks_chars(&b)),
            None => 0,
        },
        "bashExecution" => BashExecutionMessage::from_message(&Message::Custom(custom.clone()))
            .map_or(0, |bash| js_len(&bash.command) + js_len(&bash.output)),
        "branchSummary" | "compactionSummary" => custom.str_field("summary").map_or(0, js_len),
        _ => 0,
    }
}

/// Character-based token estimate (UTF-16 length / 4, rounded up); conservative.
pub fn estimate_tokens(message: &Message) -> u64 {
    let chars = match message {
        Message::System(system) => system_chars(system),
        Message::User(user) => content_chars(&user.content),
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, .. } => js_len(text),
                ContentBlock::Thinking { thinking, .. } => js_len(thinking),
                ContentBlock::ToolCall(call) => {
                    js_len(&call.name) + js_len(&serde_json::to_string(&call.arguments).unwrap_or_default())
                }
                ContentBlock::Image { .. } => 0,
            })
            .sum(),
        Message::ToolResult(result) => blocks_chars(&result.content),
        Message::Custom(custom) => custom_chars(custom),
    };
    chars.div_ceil(4) as u64
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    pub tokens: u64,
    pub usage_tokens: u64,
    pub trailing_tokens: u64,
    /// Index of the message whose usage anchors the estimate.
    pub last_usage_index: Option<usize>,
}

/// The last valid assistant usage plus estimates for the messages after it.
pub fn estimate_context_tokens(messages: &[Message]) -> ContextUsageEstimate {
    let anchor =
        messages.iter().enumerate().rev().find_map(|(index, message)| Some((index, assistant_usage(message)?)));
    match anchor {
        None => {
            let estimated = messages.iter().map(estimate_tokens).sum();
            ContextUsageEstimate {
                tokens: estimated,
                usage_tokens: 0,
                trailing_tokens: estimated,
                last_usage_index: None,
            }
        }
        Some((index, usage)) => {
            let usage_tokens = calculate_context_tokens(usage);
            let trailing_tokens = messages[index + 1..].iter().map(estimate_tokens).sum();
            ContextUsageEstimate {
                tokens: usage_tokens + trailing_tokens,
                usage_tokens,
                trailing_tokens,
                last_usage_index: Some(index),
            }
        }
    }
}

/// Context size: the last valid reported usage plus estimates for later messages.
pub fn context_tokens(messages: &[Message]) -> u64 {
    estimate_context_tokens(messages).tokens
}

/// Estimate projected context, ignoring usage captured before a later edit or compaction.
pub fn estimate_projected_context_tokens(projection: &SessionProjection, branch: &[&Entry]) -> ContextUsageEstimate {
    let estimate = estimate_context_tokens(&projection.messages);
    if let Some(usage_index) = estimate.last_usage_index {
        let mut start = 0;
        let mut usage_entry = None;
        for entry in &projection.entries {
            let end = start + entry.messages.len();
            if usage_index < end {
                usage_entry = Some(entry.source.id());
                break;
            }
            start = end;
        }
        let usage_position = usage_entry.and_then(|id| branch.iter().position(|entry| entry.id() == id));
        let invalidating = branch.iter().rposition(|entry| matches!(entry.kind(), "context_edit" | "compaction"));
        if usage_position.is_some() && usage_position.map(|p| p as i64) > invalidating.map(|p| p as i64) {
            return estimate;
        }
    }
    let system = pi_ai::transcript::current_system_message(&projection.messages);
    let mut tokens = system.map(|system| estimate_tokens(&Message::System(system))).unwrap_or(0);
    tokens += projection.messages.iter().filter(|m| !matches!(m, Message::System(_))).map(estimate_tokens).sum::<u64>();
    ContextUsageEstimate { tokens, usage_tokens: 0, trailing_tokens: tokens, last_usage_index: None }
}

/// Whether `context_tokens` crosses `context_window - reserve_tokens`.
pub fn should_compact(context_tokens: u64, context_window: u64, settings: &CompactionSettings) -> bool {
    settings.enabled && (context_tokens as i128) > context_window as i128 - settings.reserve_tokens as i128
}

/// Context use for status displays. `tokens` is `None` right after a compaction, until a
/// response reports fresh usage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}

/// Context use of the session's current branch against `context_window`.
pub fn context_usage(session: &crate::session::SessionManager, context_window: u64) -> Option<ContextUsage> {
    if context_window == 0 {
        return None;
    }
    let projection = session.build_session_projection();
    let branch = session.get_branch(None);
    if let Some(compaction) = branch.iter().rposition(|entry| entry.compaction().is_some()) {
        let fresh = projection.entries.iter().any(|projected| {
            branch[compaction + 1..].iter().any(|entry| entry.id() == projected.source.id())
                && projected.messages.iter().any(|message| assistant_usage(message).is_some())
        });
        if !fresh {
            return Some(ContextUsage { tokens: None, context_window, percent: None });
        }
    }
    let tokens = estimate_projected_context_tokens(&projection, &branch).tokens;
    Some(ContextUsage {
        tokens: Some(tokens),
        context_window,
        percent: Some(tokens as f64 / context_window as f64 * 100.0),
    })
}
