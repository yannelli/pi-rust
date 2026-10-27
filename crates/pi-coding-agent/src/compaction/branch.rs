//! Branch summaries for `/tree` navigation: summarize the branch being left so its context
//! carries over. Port of upstream `branch-summarization.ts` and `AgentSession.navigateTree`.

use std::collections::HashSet;

use pi_ai::{ContentBlock, Message, Model, StopReason, Usage};
use serde_json::Value;

use super::estimate_tokens;
use super::prepare::CompactionDetails;
use super::summarize::{SummarizationOptions, complete_summarization, request_options, summarization_context};
use super::utils::{
    FileOperations, blocks_text, compute_file_lists, extract_file_ops_from_message, format_file_operations,
    serialize_conversation, user_content_text,
};
use crate::messages::{
    convert_to_llm, create_branch_summary_message, create_compaction_summary_message, create_custom_message,
};
use crate::session::{Entry, EntryData, SessionManager};

const BRANCH_SUMMARY_PREAMBLE: &str =
    "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

const BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// `branchSummary.*` settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BranchSummarySettings {
    /// Tokens held back from the context window when choosing branch messages.
    pub reserve_tokens: u64,
    /// Skip the "Summarize branch?" question and navigate without a summary.
    pub skip_prompt: bool,
}

impl Default for BranchSummarySettings {
    fn default() -> Self {
        Self { reserve_tokens: 16_384, skip_prompt: false }
    }
}

/// Messages chosen for a branch summary, oldest first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BranchPreparation {
    pub messages: Vec<Message>,
    pub file_ops: FileOperations,
    pub total_tokens: u64,
}

/// Entries from `old_leaf` back to (not including) its deepest common ancestor with
/// `target_id`, oldest first, and that ancestor.
pub fn collect_entries_for_branch_summary<'a>(
    session: &'a SessionManager,
    old_leaf: Option<&str>,
    target_id: &str,
) -> (Vec<&'a Entry>, Option<String>) {
    let Some(old_leaf) = old_leaf else { return (Vec::new(), None) };
    let old_path: HashSet<&str> = session.get_branch(Some(old_leaf)).iter().map(|entry| entry.id()).collect();
    let target_path = session.get_branch(Some(target_id));
    let ancestor = target_path.iter().rev().find(|entry| old_path.contains(entry.id())).map(|entry| entry.id());
    let mut entries = Vec::new();
    let mut current = Some(old_leaf);
    while let Some(id) = current.filter(|id| Some(*id) != ancestor) {
        let Some(entry) = session.get_entry(id) else { break };
        entries.push(entry);
        current = entry.parent_id();
    }
    entries.reverse();
    (entries, ancestor.map(String::from))
}

fn branch_message(entry: &Entry) -> Option<Message> {
    match entry.data() {
        EntryData::Message(Message::ToolResult(_)) => None,
        EntryData::Message(message) => Some(message.clone()),
        EntryData::CustomMessage(custom) => Some(create_custom_message(
            &custom.custom_type,
            custom.content.clone(),
            custom.display,
            custom.details.clone(),
            entry.timestamp(),
        )),
        EntryData::BranchSummary(summary) => {
            Some(create_branch_summary_message(&summary.summary, &summary.from_id, entry.timestamp()))
        }
        EntryData::Compaction(compaction) => {
            Some(create_compaction_summary_message(&compaction.summary, compaction.tokens_before, entry.timestamp()))
        }
        _ => None,
    }
}

/// Newest messages first until `token_budget` (0 for no limit); summaries may squeeze in.
/// File operations come from every entry, including pi-generated nested summaries.
pub fn prepare_branch_entries(entries: &[&Entry], token_budget: u64) -> BranchPreparation {
    let mut preparation = BranchPreparation::default();
    for entry in entries {
        if let Some(summary) = entry.branch_summary()
            && summary.from_hook != Some(true)
            && let Some(details) = &summary.details
        {
            let details: CompactionDetails = serde_json::from_value(details.clone()).unwrap_or_default();
            preparation.file_ops.read.extend(details.read_files);
            preparation.file_ops.edited.extend(details.modified_files);
        }
    }
    let mut messages = Vec::new();
    for entry in entries.iter().rev() {
        let Some(message) = branch_message(entry) else { continue };
        extract_file_ops_from_message(&message, &mut preparation.file_ops);
        let tokens = estimate_tokens(&message);
        if token_budget > 0 && preparation.total_tokens + tokens > token_budget {
            let is_summary = matches!(entry.kind(), "compaction" | "branch_summary");
            if is_summary && (preparation.total_tokens as f64) < token_budget as f64 * 0.9 {
                messages.push(message);
                preparation.total_tokens += tokens;
            }
            break;
        }
        messages.push(message);
        preparation.total_tokens += tokens;
    }
    messages.reverse();
    preparation.messages = messages;
    preparation
}

#[derive(Clone, Default)]
pub struct BranchSummaryOptions {
    pub summarization: SummarizationOptions,
    pub custom_instructions: Option<String>,
    /// Use `custom_instructions` instead of the default prompt.
    pub replace_instructions: bool,
    /// Defaults to 16384 when 0.
    pub reserve_tokens: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BranchSummaryResult {
    pub summary: Option<String>,
    pub usage: Option<Usage>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
    pub aborted: bool,
    pub error: Option<String>,
}

fn failed(error: String) -> BranchSummaryResult {
    BranchSummaryResult { error: Some(error), ..Default::default() }
}

/// Summarize abandoned branch entries (oldest first).
pub async fn generate_branch_summary(
    entries: &[&Entry],
    model: &Model,
    options: &BranchSummaryOptions,
) -> BranchSummaryResult {
    let reserve = if options.reserve_tokens == 0 { 16_384 } else { options.reserve_tokens };
    let window = if model.context_window == 0 { 128_000 } else { model.context_window };
    let preparation = prepare_branch_entries(entries, window.saturating_sub(reserve));
    if preparation.messages.is_empty() {
        return BranchSummaryResult { summary: Some("No content to summarize".into()), ..Default::default() };
    }
    let conversation = serialize_conversation(&convert_to_llm(&preparation.messages));
    let instructions = match options.custom_instructions.as_deref().filter(|text| !text.is_empty()) {
        Some(custom) if options.replace_instructions => custom.to_string(),
        Some(custom) => format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {custom}"),
        None => BRANCH_SUMMARY_PROMPT.to_string(),
    };
    let prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n{instructions}");
    let summarization = &options.summarization;
    let mut request = request_options(model, summarization);
    request.session_id = None;
    let response = complete_summarization(
        model,
        &summarization_context(prompt),
        request,
        summarization.stream_fn.as_ref(),
        summarization.retry.as_ref(),
        summarization.on_retry.as_ref(),
    )
    .await;
    if response.stop_reason == StopReason::Aborted {
        return BranchSummaryResult { aborted: true, ..Default::default() };
    }
    if let Some(failure) = super::summarization_failure(&response, "Branch summarization") {
        return failed(failure);
    }
    if response.content.iter().any(|block| matches!(block, ContentBlock::ToolCall(_))) {
        return failed("Branch summarization attempted to call a tool".into());
    }
    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    let mut summary = format!("{BRANCH_SUMMARY_PREAMBLE}{}", blocks_text(&response.content, "\n"));
    summary.push_str(&format_file_operations(&read_files, &modified_files));
    BranchSummaryResult {
        summary: Some(summary),
        usage: Some(response.usage),
        read_files,
        modified_files,
        aborted: false,
        error: None,
    }
}

/// A summary supplied by an extension instead of the default summarizer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProvidedSummary {
    pub summary: String,
    pub details: Option<Value>,
    pub usage: Option<Usage>,
}

/// What the user chose in `/tree`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeNavigationRequest {
    pub target_id: String,
    pub summarize: bool,
    pub label: Option<String>,
    /// Used instead of generating a summary when `summarize` is set.
    pub provided_summary: Option<ProvidedSummary>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeNavigation {
    /// Text to put back in the editor when the target was a user or custom message.
    pub editor_text: Option<String>,
    pub cancelled: bool,
    pub aborted: bool,
    pub summary_entry_id: Option<String>,
}

type PendingSummary = (String, Option<Value>, Option<Usage>, bool);

async fn branch_summary_for(
    entries: &[Entry],
    request: &mut TreeNavigationRequest,
    model: Option<&Model>,
    options: &BranchSummaryOptions,
) -> anyhow::Result<Result<Option<PendingSummary>, TreeNavigation>> {
    if !request.summarize {
        return Ok(Ok(None));
    }
    if let Some(provided) = request.provided_summary.take() {
        return Ok(Ok(Some((provided.summary, provided.details, provided.usage, true))));
    }
    if entries.is_empty() {
        return Ok(Ok(None));
    }
    let Some(model) = model else { anyhow::bail!("No model available for summarization") };
    let refs: Vec<&Entry> = entries.iter().collect();
    let result = generate_branch_summary(&refs, model, options).await;
    if result.aborted {
        return Ok(Err(TreeNavigation { cancelled: true, aborted: true, ..Default::default() }));
    }
    if let Some(error) = result.error {
        anyhow::bail!(error);
    }
    let details = CompactionDetails { read_files: result.read_files, modified_files: result.modified_files };
    Ok(Ok(result.summary.map(|text| (text, serde_json::to_value(details).ok(), result.usage, false))))
}

/// Move the leaf to `request.target_id`, summarizing the abandoned branch when asked.
/// Selecting a user or custom message moves to its parent and returns its text.
pub async fn navigate_tree(
    session: &mut SessionManager,
    mut request: TreeNavigationRequest,
    model: Option<&Model>,
    options: &BranchSummaryOptions,
) -> anyhow::Result<TreeNavigation> {
    let old_leaf = session.leaf_id().map(String::from);
    if old_leaf.as_deref() == Some(request.target_id.as_str()) {
        return Ok(TreeNavigation::default());
    }
    let Some(target) = session.get_entry(&request.target_id).cloned() else {
        anyhow::bail!("Entry {} not found", request.target_id)
    };
    let (collected, _) = collect_entries_for_branch_summary(session, old_leaf.as_deref(), &request.target_id);
    let entries: Vec<Entry> = collected.into_iter().cloned().collect();
    let summary = match branch_summary_for(&entries, &mut request, model, options).await? {
        Ok(summary) => summary,
        Err(cancelled) => return Ok(cancelled),
    };
    let (new_leaf, editor_text) = match target.data() {
        EntryData::Message(Message::User(user)) => {
            (target.parent_id().map(String::from), Some(user_content_text(&user.content, "")))
        }
        EntryData::CustomMessage(custom) => {
            (target.parent_id().map(String::from), Some(user_content_text(&custom.content, "")))
        }
        _ => (Some(request.target_id.clone()), None),
    };
    let label = request.label.filter(|label| !label.is_empty());
    let mut summary_entry_id = None;
    match summary {
        Some((text, details, usage, from_hook)) => {
            let id = session.branch_with_summary(new_leaf.as_deref(), &text, details, Some(from_hook), usage)?;
            if let Some(label) = &label {
                session.append_label_change(&id, Some(label))?;
            }
            summary_entry_id = Some(id);
        }
        None => {
            match &new_leaf {
                Some(id) => session.branch(id)?,
                None => session.reset_leaf(),
            }
            if let Some(label) = &label {
                session.append_label_change(&request.target_id, Some(label))?;
            }
        }
    }
    Ok(TreeNavigation { editor_text, cancelled: false, aborted: false, summary_entry_id })
}
