//! The model-written `/bug` summary, used when the user does not attach the transcript
//! (upstream `generateBugReportSummary` in `core/bug-report.ts`).

use std::collections::BTreeMap;

use pi_agent::StreamFn;
use pi_ai::{
    CacheRetention, ContentBlock, Context, Message, Model, StopReason, StreamOptions, ThinkingLevel, UserContent,
    UserMessage,
};
use tokio_util::sync::CancellationToken;

use crate::compaction::utils::blocks_text;
use crate::compaction::{complete_summarization, estimate_tokens, serialize_conversation, summarization_failure};
use crate::messages::convert_to_llm;
use pi_ai::retry::RetryPolicy;

pub const BUG_SUMMARY_SYSTEM_PROMPT: &str = "You are helping a user file a bug report about pi, the coding agent they are talking to. You will be shown the conversation transcript. Write a report for the pi developers describing what the user was doing and what went wrong.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the report.";

pub const BUG_SUMMARY_INSTRUCTIONS: &str = "Write the bug report in Markdown with these sections:

## What the user was doing
One short paragraph.

## What went wrong
Concrete description of the failure: wrong output, errors, hangs, tool failures, unexpected behavior. Quote error messages and tool output verbatim where they exist.

## Steps to reproduce
Numbered list, as specific as the transcript allows.

## Relevant details
Tool calls involved, files touched, model behavior, anything else that helps a developer reproduce or locate the problem.

Do not include file contents, secrets, or credentials from the transcript; refer to files by path only. Keep the report factual and concise.";

/// The newest messages that fit `token_budget`, always at least one.
pub fn select_messages(messages: &[Message], token_budget: u64) -> &[Message] {
    let mut tokens = 0;
    let mut start = messages.len();
    while start > 0 {
        let next = estimate_tokens(&messages[start - 1]);
        if start < messages.len() && tokens + next > token_budget {
            break;
        }
        tokens += next;
        start -= 1;
    }
    &messages[start..]
}

/// The user prompt: an omission note, the transcript, the user's description, and the format.
pub fn bug_summary_prompt(messages: &[Message], hint: Option<&str>, context_window: u64) -> String {
    let window = if context_window > 0 { context_window } else { 128_000 };
    let selected = select_messages(messages, window * 6 / 10);
    let mut parts = Vec::new();
    if selected.len() < messages.len() {
        parts.push(format!("Note: only the last {} of {} messages are shown.", selected.len(), messages.len()));
    }
    parts.push(format!("<conversation>\n{}\n</conversation>", serialize_conversation(&convert_to_llm(selected))));
    if let Some(hint) = hint.map(str::trim).filter(|hint| !hint.is_empty()) {
        parts.push(format!("<user-report>\n{hint}\n</user-report>"));
    }
    parts.push(BUG_SUMMARY_INSTRUCTIONS.into());
    parts.join("\n\n")
}

#[derive(Clone, Default)]
pub struct BugReportSummaryOptions<'a> {
    pub messages: &'a [Message],
    pub hint: Option<&'a str>,
    pub model: Option<&'a Model>,
    pub api_key: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    pub cancel: CancellationToken,
    pub thinking_level: Option<ThinkingLevel>,
    pub stream_fn: Option<&'a StreamFn>,
    pub retry: Option<&'a RetryPolicy>,
    pub session_id: Option<String>,
}

/// Ask the session model for the report.
pub async fn generate_bug_report_summary(options: BugReportSummaryOptions<'_>) -> Result<String, String> {
    let model = options.model.ok_or("No model selected")?;
    let prompt = bug_summary_prompt(options.messages, options.hint, model.context_window);
    let reasoning = options.thinking_level.filter(|level| model.reasoning && *level != ThinkingLevel::Off);
    let max_tokens = if model.max_tokens > 0 { model.max_tokens.min(4096) } else { 4096 };
    let stream_options = StreamOptions {
        api_key: options.api_key,
        cancel: options.cancel.clone(),
        max_tokens: Some(max_tokens),
        reasoning,
        headers: options.headers,
        env: options.env,
        session_id: options.session_id,
        cache_retention: Some(CacheRetention::None),
        ..StreamOptions::default()
    };
    let context = Context {
        system_prompt: Some(BUG_SUMMARY_SYSTEM_PROMPT.into()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Blocks(vec![ContentBlock::text(prompt)]),
            timestamp: pi_ai::now_ms(),
        })],
        tools: Vec::new(),
    };
    let response =
        complete_summarization(model, &context, stream_options, options.stream_fn, options.retry, None).await;
    if response.stop_reason == StopReason::Aborted {
        return Err("Bug report summary was cancelled".into());
    }
    if let Some(failure) = summarization_failure(&response, "Bug report summary") {
        return Err(failure);
    }
    if response.content.iter().any(|block| matches!(block, ContentBlock::ToolCall(_))) {
        return Err("Bug report summary attempted to call a tool".into());
    }
    let text = blocks_text(&response.content, "\n").trim().to_string();
    if text.is_empty() {
        return Err("Bug report summary was empty".into());
    }
    Ok(text)
}
