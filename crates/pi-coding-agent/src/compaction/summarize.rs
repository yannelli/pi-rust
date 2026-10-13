//! Summary generation for compaction, including split turns, and the entry points the app
//! calls. Prompts match upstream `compaction.ts`.

use std::collections::BTreeMap;
use std::sync::Arc;

use pi_agent::StreamFn;
use pi_ai::retry::{RetryEvent, RetryPolicy, retry_assistant_call};
use pi_ai::{
    AssistantMessage, CacheRetention, ContentBlock, Context, Message, Model, StopReason, StreamOptions, ThinkingLevel,
    Usage, UserContent, UserMessage,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::prepare::{CompactionDetails, CompactionPreparation};
use super::utils::{SUMMARIZATION_SYSTEM_PROMPT, blocks_text, compute_file_lists, format_file_operations};
use super::{CompactionSettings, estimate_tokens, serialize_conversation};
use crate::messages::convert_to_llm;
use crate::session::usage::combine_usage;
use crate::session::{NewCompaction, SessionManager};

const SUMMARY_FORMAT_TAIL: &str =
    "Keep each section concise. Preserve exact file paths, function names, and error messages.";

const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\n";

const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\n";

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.\n\nCreate a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later messages so the conversation can continue with the necessary context.\n\n## Original Request\n[What did the user ask for?]\n\n## Progress So Far\n- [Key decisions and work completed in these messages]\n\n## Context Needed to Continue\n- [Information from these messages needed to understand the later work]\n\nOnly summarize information explicitly present above. Do not infer or recreate later messages.";

/// Receives retry progress of summarization calls.
pub type RetryEventFn = Arc<dyn Fn(RetryEvent) + Send + Sync>;

/// Request settings shared by every summarization call.
#[derive(Clone, Default)]
pub struct SummarizationOptions {
    pub api_key: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    pub cancel: CancellationToken,
    /// Reasoning level for models that support it.
    pub thinking_level: Option<ThinkingLevel>,
    /// The session's stream function, so requests behave like agent requests.
    pub stream_fn: Option<StreamFn>,
    pub retry: Option<RetryPolicy>,
    /// Retry progress, for example the session's `summarization_retry_*` events.
    pub on_retry: Option<RetryEventFn>,
    /// Routing id; a fresh one is used when absent.
    pub session_id: Option<String>,
}

/// The outcome of a compaction.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub estimated_tokens_after: Option<u64>,
    /// Usage of the summarization calls.
    pub usage: Usage,
    /// `{ readFiles, modifiedFiles }` for pi-generated summaries.
    pub details: Option<Value>,
}

/// Why a summarization response cannot be stored: an error, or a `length` stop (partial text).
pub fn summarization_failure(response: &AssistantMessage, label: &str) -> Option<String> {
    match response.stop_reason {
        StopReason::Error => Some(format!(
            "{label} failed: {}",
            response.error_message.as_deref().filter(|error| !error.is_empty()).unwrap_or("Unknown error")
        )),
        StopReason::Length => {
            Some(format!("{label} failed: generation hit the token cap and the summary is incomplete"))
        }
        _ => None,
    }
}

pub(crate) fn request_options(model: &Model, max_tokens: u64, options: &SummarizationOptions) -> StreamOptions {
    let reasoning = options.thinking_level.filter(|level| model.reasoning && *level != ThinkingLevel::Off);
    StreamOptions {
        api_key: options.api_key.clone(),
        cancel: options.cancel.clone(),
        max_tokens: Some(max_tokens),
        reasoning,
        headers: options.headers.clone(),
        env: options.env.clone(),
        session_id: options.session_id.clone(),
        ..StreamOptions::default()
    }
}

/// One summarization request with prompt-cache writes disabled, retried on transient errors.
pub async fn complete_summarization(
    model: &Model,
    context: &Context,
    mut options: StreamOptions,
    stream_fn: Option<&StreamFn>,
    retry: Option<&RetryPolicy>,
    on_retry: Option<&RetryEventFn>,
) -> AssistantMessage {
    options.cache_retention = Some(CacheRetention::None);
    options.session_id = options.session_id.or_else(|| Some(crate::session::uuid_v7()));
    let cancel = options.cancel.clone();
    let produce = || {
        let (model, context, options, stream_fn) = (model.clone(), context.clone(), options.clone(), stream_fn.cloned());
        async move {
            match stream_fn {
                Some(stream_fn) => stream_fn(&model, &context, options).result(&model).await,
                None => pi_ai::complete(&model, &context, options).await,
            }
        }
    };
    let mut on_event = |event: RetryEvent| {
        if let Some(callback) = on_retry {
            callback(event);
        }
    };
    retry_assistant_call(produce, retry, &cancel, &mut on_event).await
}

pub(crate) fn summarization_context(prompt: String) -> Context {
    Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.into()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Blocks(vec![ContentBlock::text(prompt)]),
            timestamp: pi_ai::now_ms(),
        })],
        tools: Vec::new(),
    }
}

fn output_cap(model: &Model, budget: u64) -> u64 {
    if model.max_tokens > 0 { budget.min(model.max_tokens) } else { budget }
}

fn checked_text(response: &AssistantMessage, label: &str) -> anyhow::Result<(String, Usage)> {
    if let Some(failure) = summarization_failure(response, label) {
        anyhow::bail!(failure);
    }
    if response.content.iter().any(|block| matches!(block, ContentBlock::ToolCall(_))) {
        anyhow::bail!("{label} attempted to call a tool");
    }
    Ok((blocks_text(&response.content, "\n"), response.usage))
}

/// Summarize `messages`, merging into `previous_summary` when given.
pub async fn generate_summary_with_usage(
    messages: &[Message],
    model: &Model,
    reserve_tokens: u64,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
    options: &SummarizationOptions,
) -> anyhow::Result<(String, Usage)> {
    let max_tokens = output_cap(model, reserve_tokens * 8 / 10);
    let mut instructions =
        String::from(if previous_summary.is_some() { UPDATE_SUMMARIZATION_PROMPT } else { SUMMARIZATION_PROMPT });
    instructions.push_str(SUMMARY_FORMAT_TAIL);
    if let Some(focus) = custom_instructions.filter(|focus| !focus.is_empty()) {
        instructions.push_str(&format!("\n\nAdditional focus: {focus}"));
    }
    let conversation = serialize_conversation(&convert_to_llm(messages));
    let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
    if let Some(previous) = previous_summary {
        prompt.push_str(&format!("<previous-summary>\n{previous}\n</previous-summary>\n\n"));
    }
    prompt.push_str(&instructions);
    let request = request_options(model, max_tokens, options);
    let response = complete_summarization(
        model,
        &summarization_context(prompt),
        request,
        options.stream_fn.as_ref(),
        options.retry.as_ref(),
        options.on_retry.as_ref(),
    )
    .await;
    checked_text(&response, "Summarization")
}

/// [`generate_summary_with_usage`] without the usage.
pub async fn generate_summary(
    messages: &[Message],
    model: &Model,
    reserve_tokens: u64,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
    options: &SummarizationOptions,
) -> anyhow::Result<String> {
    Ok(generate_summary_with_usage(messages, model, reserve_tokens, custom_instructions, previous_summary, options)
        .await?
        .0)
}

async fn generate_turn_prefix_summary(
    messages: &[Message],
    model: &Model,
    reserve_tokens: u64,
    options: &SummarizationOptions,
) -> anyhow::Result<(String, Usage)> {
    let max_tokens = output_cap(model, reserve_tokens / 2);
    let conversation = serialize_conversation(&convert_to_llm(messages));
    let prompt = format!("# Conversation\n{conversation}\n\n# Instructions\n{TURN_PREFIX_SUMMARIZATION_PROMPT}");
    let request = request_options(model, max_tokens, options);
    let response = complete_summarization(
        model,
        &summarization_context(prompt),
        request,
        options.stream_fn.as_ref(),
        options.retry.as_ref(),
        options.on_retry.as_ref(),
    )
    .await;
    checked_text(&response, "Turn prefix summarization")
}

/// Generate the compaction summary for `preparation` (upstream `compact`). A split turn gets
/// a history summary and a turn prefix summary, merged into one.
pub async fn summarize_compaction(
    preparation: &CompactionPreparation,
    model: &Model,
    custom_instructions: Option<&str>,
    options: &SummarizationOptions,
) -> anyhow::Result<CompactionResult> {
    let reserve = preparation.settings.reserve_tokens;
    let previous = preparation.previous_summary.as_deref();
    let (mut summary, usage) = if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
        let (history, history_usage) = if preparation.messages_to_summarize.is_empty() {
            (previous.unwrap_or("No prior history.").to_string(), None)
        } else {
            let (text, usage) = generate_summary_with_usage(
                &preparation.messages_to_summarize,
                model,
                reserve,
                custom_instructions,
                previous,
                options,
            )
            .await?;
            (text, Some(usage))
        };
        let (prefix, prefix_usage) =
            generate_turn_prefix_summary(&preparation.turn_prefix_messages, model, reserve, options).await?;
        let usage = history_usage.map_or(prefix_usage, |usage| combine_usage(&usage, &prefix_usage));
        (format!("{history}\n\n---\n\n**Turn Context (split turn):**\n\n{prefix}"), usage)
    } else {
        generate_summary_with_usage(
            &preparation.messages_to_summarize,
            model,
            reserve,
            custom_instructions,
            previous,
            options,
        )
        .await?
    };
    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    summary.push_str(&format_file_operations(&read_files, &modified_files));
    let details = serde_json::to_value(CompactionDetails { read_files, modified_files }).ok();
    Ok(CompactionResult {
        summary,
        first_kept_entry_id: preparation.first_kept_entry_id.clone(),
        tokens_before: preparation.tokens_before,
        estimated_tokens_after: None,
        usage,
        details,
    })
}

/// Summarize per `preparation` and append the compaction entry (with details and usage).
pub async fn compact_session(
    session: &mut SessionManager,
    preparation: &CompactionPreparation,
    model: &Model,
    custom_instructions: Option<&str>,
    options: &SummarizationOptions,
) -> anyhow::Result<CompactionResult> {
    let mut result = summarize_compaction(preparation, model, custom_instructions, options).await?;
    if options.cancel.is_cancelled() {
        anyhow::bail!("Compaction cancelled");
    }
    session.append_compaction_with(NewCompaction {
        summary: result.summary.clone(),
        first_kept_entry_id: Some(result.first_kept_entry_id.clone()),
        tokens_before: result.tokens_before,
        details: result.details.clone(),
        from_hook: Some(false),
        usage: Some(result.usage),
    })?;
    result.estimated_tokens_after = Some(session.build_session_context().messages.iter().map(estimate_tokens).sum());
    Ok(result)
}

/// Summarize per `plan` with `model` and record a compaction entry.
pub async fn compact(
    session: &mut SessionManager,
    plan: CompactionPreparation,
    model: &Model,
    api_key: Option<String>,
    instructions: Option<&str>,
    settings: &CompactionSettings,
    cancel: CancellationToken,
) -> anyhow::Result<CompactionResult> {
    let plan = CompactionPreparation { settings: *settings, ..plan };
    let options = SummarizationOptions { api_key, cancel, ..SummarizationOptions::default() };
    compact_session(session, &plan, model, instructions, &options).await
}
