//! Context compaction: summarize older turns into a checkpoint so long sessions
//! fit the model's context window. Prompts match the TypeScript pi.

use pi_ai::{AssistantMessage, ContentBlock, Context, Message, Model, StopReason, StreamOptions, Usage, UserContent};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::session::{Entry, SessionManager};

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARY_FORMAT: &str = "Use this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_RULES: &str = "Update the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it";

const TOOL_RESULT_MAX_CHARS: usize = 2000;

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

/// Character-based estimate (4 characters per token).
pub fn estimate_tokens(message: &Message) -> u64 {
    let chars: usize = match message {
        Message::System(system) => system.content.text().len(),
        Message::Custom(custom) => Value::Object(custom.fields.clone()).to_string().len(),
        Message::User(user) => match &user.content {
            UserContent::Text(text) => text.len(),
            UserContent::Blocks(blocks) => blocks.iter().map(block_chars).sum(),
        },
        Message::Assistant(assistant) => assistant.content.iter().map(block_chars).sum(),
        Message::ToolResult(result) => result.content.iter().map(block_chars).sum(),
    };
    chars.div_ceil(4) as u64
}

fn block_chars(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text, .. } => text.len(),
        ContentBlock::Thinking { thinking, .. } => thinking.len(),
        ContentBlock::ToolCall(call) => call.name.len() + Value::Object(call.arguments.clone()).to_string().len(),
        ContentBlock::Image { .. } => 4800,
    }
}

fn usage_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

/// Context size: the last valid reported usage plus estimates for later messages.
pub fn context_tokens(messages: &[Message]) -> u64 {
    let last_usage = messages.iter().enumerate().rev().find_map(|(index, message)| match message {
        Message::Assistant(assistant)
            if !matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted)
                && usage_tokens(&assistant.usage) > 0 =>
        {
            Some((index, usage_tokens(&assistant.usage)))
        }
        _ => None,
    });
    match last_usage {
        Some((index, tokens)) => tokens + messages[index + 1..].iter().map(estimate_tokens).sum::<u64>(),
        None => messages.iter().map(estimate_tokens).sum(),
    }
}

pub fn should_compact(tokens: u64, context_window: u64, settings: &CompactionSettings) -> bool {
    settings.enabled && tokens > context_window.saturating_sub(settings.reserve_tokens)
}

/// What a compaction will summarize and keep.
#[derive(Clone, Debug)]
pub struct CompactionPlan {
    pub first_kept_entry_id: String,
    pub messages_to_summarize: Vec<Message>,
    pub previous_summary: Option<String>,
    pub tokens_before: u64,
}

/// Choose the cut point: keep roughly `keep_recent_tokens` of the newest history,
/// cutting only at user or assistant messages so tool results stay with their calls.
pub fn prepare_compaction(session: &SessionManager, settings: &CompactionSettings) -> Option<CompactionPlan> {
    let branch = session.branch();
    let compaction = branch.iter().rposition(|entry| entry.kind == "compaction");
    let (previous_summary, range): (Option<String>, Vec<&Entry>) = match compaction {
        None => (None, branch.clone()),
        Some(index) => {
            let first_kept = branch[index].data.get("firstKeptEntryId").and_then(Value::as_str).unwrap_or_default();
            let kept = branch[..index].iter().skip_while(|entry| entry.id != first_kept).copied();
            let summary = branch[index].data.get("summary").and_then(Value::as_str).map(String::from);
            (summary, kept.chain(branch[index + 1..].iter().copied()).collect())
        }
    };
    let messages: Vec<(usize, Message)> =
        range.iter().enumerate().filter_map(|(index, entry)| entry.message().map(|message| (index, message))).collect();
    let tokens_before = context_tokens(&session.context().messages);
    let is_cut_point = |message: &Message| matches!(message, Message::User(_) | Message::Assistant(_));
    let mut accumulated = 0;
    let mut cut = None;
    for (position, (_, message)) in messages.iter().enumerate().rev() {
        accumulated += estimate_tokens(message);
        if accumulated >= settings.keep_recent_tokens {
            cut = messages[position..].iter().find(|(_, message)| is_cut_point(message)).map(|(index, _)| *index);
            break;
        }
    }
    let cut = cut?;
    let messages_to_summarize: Vec<Message> =
        messages.iter().filter(|(index, _)| *index < cut).map(|(_, message)| message.clone()).collect();
    if messages_to_summarize.is_empty() {
        return None;
    }
    Some(CompactionPlan {
        first_kept_entry_id: range[cut].id.clone(),
        messages_to_summarize,
        previous_summary,
        tokens_before,
    })
}

/// Render messages as labeled text so the summarizer does not continue the conversation.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let text_of =
        |blocks: &[ContentBlock]| blocks.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n");
    let mut parts = Vec::new();
    for message in messages {
        match message {
            Message::System(_) | Message::Custom(_) => {}
            Message::User(user) => {
                let text = user.content.text();
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(assistant) => {
                let thinking: Vec<&str> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                        _ => None,
                    })
                    .collect();
                if !thinking.is_empty() {
                    parts.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
                }
                if assistant.content.iter().any(|block| block.as_text().is_some()) {
                    parts.push(format!("[Assistant]: {}", assistant.text()));
                }
                let calls: Vec<String> = assistant
                    .tool_calls()
                    .map(|call| {
                        let args: Vec<String> =
                            call.arguments.iter().map(|(key, value)| format!("{key}={value}")).collect();
                        format!("{}({})", call.name, args.join(", "))
                    })
                    .collect();
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let text = text_of(&result.content);
                if !text.is_empty() {
                    let chars = text.chars().count();
                    let text = if chars > TOOL_RESULT_MAX_CHARS {
                        let kept: String = text.chars().take(TOOL_RESULT_MAX_CHARS).collect();
                        format!("{kept}\n\n[... {} more characters truncated]", chars - TOOL_RESULT_MAX_CHARS)
                    } else {
                        text
                    };
                    parts.push(format!("[Tool result]: {text}"));
                }
            }
        }
    }
    parts.join("\n\n")
}

/// The summarization request, in the TypeScript pi format.
pub fn summary_request(messages: &[Message], previous_summary: Option<&str>, instructions: Option<&str>) -> Context {
    let mut prompt = format!("<conversation>\n{}\n</conversation>\n\n", serialize_conversation(messages));
    if let Some(previous) = previous_summary {
        prompt.push_str(&format!("<previous-summary>\n{previous}\n</previous-summary>\n\n"));
        prompt.push_str("The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\n");
        prompt.push_str(UPDATE_RULES);
        prompt.push_str("\n\n");
    } else {
        prompt.push_str("The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\n");
    }
    prompt.push_str(SUMMARY_FORMAT);
    if let Some(instructions) = instructions.filter(|text| !text.trim().is_empty()) {
        prompt.push_str(&format!("\n\nAdditional focus: {instructions}"));
    }
    Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.into()),
        messages: vec![Message::User(pi_ai::UserMessage {
            content: UserContent::Blocks(vec![ContentBlock::text(prompt)]),
            timestamp: pi_ai::now_ms(),
        })],
        tools: Vec::new(),
    }
}

#[derive(Clone, Debug)]
pub struct CompactionResult {
    pub summary: String,
    pub tokens_before: u64,
    pub usage: Usage,
}

/// Summarize per `plan` with `model` and record a compaction entry.
pub async fn compact(
    session: &mut SessionManager,
    plan: CompactionPlan,
    model: &Model,
    api_key: Option<String>,
    instructions: Option<&str>,
    settings: &CompactionSettings,
    cancel: CancellationToken,
) -> anyhow::Result<CompactionResult> {
    let request = summary_request(&plan.messages_to_summarize, plan.previous_summary.as_deref(), instructions);
    let max_tokens = (settings.reserve_tokens * 8 / 10).min(model.max_tokens.max(1));
    let options = StreamOptions { api_key, cancel, max_tokens: Some(max_tokens), ..StreamOptions::default() };
    let response: AssistantMessage = pi_ai::complete(model, &request, options).await;
    match response.stop_reason {
        StopReason::Stop => {}
        StopReason::Length => anyhow::bail!("Summarization hit the output token limit"),
        _ => anyhow::bail!("Summarization failed: {}", response.error_message.as_deref().unwrap_or("unknown error")),
    }
    let summary = response.text().trim().to_string();
    if summary.is_empty() {
        anyhow::bail!("Summarization returned no text");
    }
    session.append_compaction(&summary, &plan.first_kept_entry_id, plan.tokens_before)?;
    Ok(CompactionResult { summary, tokens_before: plan.tokens_before, usage: response.usage })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pi_ai::providers::faux::{FauxProvider, FauxResponse};

    use super::*;

    fn assistant(text: &str) -> Message {
        let mut message = AssistantMessage::empty(&FauxProvider::model());
        message.content.push(ContentBlock::text(text));
        message.stop_reason = StopReason::Stop;
        Message::Assistant(message)
    }

    #[test]
    fn estimates_and_thresholds() {
        assert_eq!(estimate_tokens(&Message::user("12345678")), 2);
        let mut with_usage = AssistantMessage::empty(&FauxProvider::model());
        with_usage.usage.total_tokens = 100;
        with_usage.stop_reason = StopReason::Stop;
        let messages = vec![Message::Assistant(with_usage), Message::user("1234")];
        assert_eq!(context_tokens(&messages), 101);
        let settings = CompactionSettings::default();
        assert!(should_compact(120_000, 128_000, &settings));
        assert!(!should_compact(100_000, 128_000, &settings));
    }

    #[tokio::test]
    async fn compacts_older_turns_into_a_summary_entry() {
        let mut session = SessionManager::create(Path::new("/w"), None);
        for turn in 0..6 {
            session.append_message(&Message::user(format!("question {turn} {}", "x".repeat(400)))).unwrap();
            session.append_message(&assistant(&format!("answer {turn} {}", "y".repeat(400)))).unwrap();
        }
        let settings = CompactionSettings { keep_recent_tokens: 250, ..CompactionSettings::default() };
        let plan = prepare_compaction(&session, &settings).unwrap();
        assert!(!plan.messages_to_summarize.is_empty() && plan.messages_to_summarize.len() < 12);
        let provider = FauxProvider::new([FauxResponse::text("## Goal\nsummary")]);
        pi_ai::register_api_provider("faux-compaction", provider.clone());
        let mut model = FauxProvider::model();
        model.api = "faux-compaction".into();
        let result =
            compact(&mut session, plan.clone(), &model, None, Some("focus"), &settings, CancellationToken::new())
                .await
                .unwrap();
        assert_eq!(result.summary, "## Goal\nsummary");
        let request = &provider.requests()[0];
        assert_eq!(request.system_prompt.as_deref(), Some(SUMMARIZATION_SYSTEM_PROMPT));
        let prompt = match &request.messages[0] {
            Message::User(user) => user.content.text(),
            _ => String::new(),
        };
        assert!(prompt.starts_with("<conversation>\n[User]: question 0"));
        assert!(prompt.ends_with("Additional focus: focus"));
        let context = session.context();
        assert!(matches!(&context.messages[0], Message::User(user) if user.content.text().contains("summary")));
        assert_eq!(context.messages.len(), 12 - plan.messages_to_summarize.len() + 1);
        let second = prepare_compaction(&session, &settings);
        assert!(second.is_none_or(|plan| plan.previous_summary.as_deref() == Some("## Goal\nsummary")));
    }
}
