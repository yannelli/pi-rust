//! Compaction preparation: which messages the summary replaces, which stay, and the file
//! operations to carry into the summary. Port of upstream `prepareCompaction`.

use pi_ai::Message;
use serde::{Deserialize, Serialize};

use super::cut_point::find_projected_cut_point;
use super::utils::{FileOperations, extract_file_ops_from_message};
use super::{CompactionSettings, estimate_projected_context_tokens};
use crate::session::{Entry, ProjectedEntry, SessionManager, context};

/// `details` of a pi-generated compaction or branch summary entry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    #[serde(default)]
    pub read_files: Vec<String>,
    #[serde(default)]
    pub modified_files: Vec<String>,
}

/// The plan for one compaction.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionPreparation {
    /// Id of the first entry kept verbatim.
    pub first_kept_entry_id: String,
    /// Messages replaced by the summary.
    pub messages_to_summarize: Vec<Message>,
    /// The start of a split turn, summarized separately.
    pub turn_prefix_messages: Vec<Message>,
    pub is_split_turn: bool,
    pub tokens_before: u64,
    /// Summary of the compaction this one extends.
    pub previous_summary: Option<String>,
    pub file_ops: FileOperations,
    pub settings: CompactionSettings,
}

fn conversation_messages(entry: &ProjectedEntry) -> Vec<Message> {
    if entry.source.compaction().is_some() {
        return Vec::new();
    }
    // System messages are prompt state; the compaction entry carries their replay.
    entry.messages.iter().filter(|message| !matches!(message, Message::System(_))).cloned().collect()
}

/// Prepare a compaction of the session's current branch.
pub fn prepare_compaction(session: &SessionManager, settings: &CompactionSettings) -> Option<CompactionPreparation> {
    prepare_compaction_entries(&session.get_branch(None), settings)
}

/// Prepare a compaction of `path_entries` (root to leaf). `None` when the branch already ends
/// in a compaction or the summary would replace nothing.
pub fn prepare_compaction_entries(
    path_entries: &[&Entry],
    settings: &CompactionSettings,
) -> Option<CompactionPreparation> {
    if path_entries.last().is_some_and(|entry| entry.compaction().is_some()) {
        return None;
    }
    let projection = context::project_path(context::path_to_last(path_entries));
    let entries = &projection.entries;
    // The newest compaction is projected first; older ones in its kept range contribute nothing.
    let previous_index =
        entries.iter().position(|entry| entry.source.compaction().is_some() && !entry.messages.is_empty());
    let previous_compaction = previous_index.and_then(|index| entries[index].source.compaction());
    let previous_summary = previous_compaction.map(|compaction| compaction.summary.clone());
    let boundary_start = previous_index.map_or(0, |index| index + 1);
    let tokens_before = estimate_projected_context_tokens(&projection, path_entries).tokens;
    let cut = find_projected_cut_point(entries, boundary_start, entries.len(), settings.keep_recent_tokens);
    let first_kept = entries.get(cut.first_kept_entry_index)?.source;
    if first_kept.id().is_empty() {
        return None;
    }
    let history_end = if cut.is_split_turn { cut.turn_start_index? } else { cut.first_kept_entry_index };
    let messages_to_summarize: Vec<Message> =
        entries[boundary_start.min(history_end)..history_end].iter().flat_map(conversation_messages).collect();
    let turn_prefix_messages: Vec<Message> = match cut.turn_start_index.filter(|_| cut.is_split_turn) {
        Some(turn_start) => {
            entries[turn_start..cut.first_kept_entry_index].iter().flat_map(conversation_messages).collect()
        }
        None => Vec::new(),
    };
    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }
    let mut file_ops = FileOperations::default();
    if let Some(compaction) = previous_compaction
        && compaction.from_hook != Some(true)
        && let Some(details) = &compaction.details
    {
        let list = |key: &str| -> Vec<String> {
            details.get(key).and_then(|value| serde_json::from_value(value.clone()).ok()).unwrap_or_default()
        };
        file_ops.read.extend(list("readFiles"));
        file_ops.edited.extend(list("modifiedFiles"));
    }
    for message in messages_to_summarize.iter().chain(&turn_prefix_messages) {
        extract_file_ops_from_message(message, &mut file_ops);
    }
    Some(CompactionPreparation {
        first_kept_entry_id: first_kept.id().to_string(),
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: cut.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings: *settings,
    })
}
