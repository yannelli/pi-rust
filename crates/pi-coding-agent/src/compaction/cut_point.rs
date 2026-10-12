//! Where to cut: keep roughly `keep_recent_tokens` of the newest context, cutting only at
//! user-like or assistant messages so tool results stay with their calls.

use pi_ai::Message;

use super::estimate_tokens;
use crate::session::{Entry, EntryData, ProjectedEntry, session_entry_to_context_messages};

/// Messages a cut may start at: everything but tool results and system messages.
pub fn is_cut_point_message(message: &Message) -> bool {
    match message {
        Message::User(_) | Message::Assistant(_) => true,
        Message::Custom(custom) => {
            matches!(custom.role.as_str(), "bashExecution" | "custom" | "branchSummary" | "compactionSummary")
        }
        Message::ToolResult(_) | Message::System(_) => false,
    }
}

/// Messages that start a turn: user-like input, never assistant output.
pub fn is_turn_start_message(message: &Message) -> bool {
    !matches!(message, Message::Assistant(_)) && is_cut_point_message(message)
}

fn is_turn_start_entry(entry: &Entry) -> bool {
    entry.compaction().is_none() && session_entry_to_context_messages(entry).iter().any(is_turn_start_message)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutPointResult {
    /// Index of the first entry to keep.
    pub first_kept_entry_index: usize,
    /// Index of the entry that starts the turn being split.
    pub turn_start_index: Option<usize>,
    /// Whether the cut lands inside a turn (at an assistant message).
    pub is_split_turn: bool,
}

/// The turn-starting entry at or before `entry_index`, not before `start`.
pub fn find_turn_start_index(entries: &[&Entry], entry_index: usize, start: usize) -> Option<usize> {
    (start..=entry_index).rev().find(|index| is_turn_start_entry(entries[*index]))
}

fn split_result(cut: usize, starts_turn: bool, turn_start: impl FnOnce() -> Option<usize>) -> CutPointResult {
    let turn_start_index = if starts_turn { None } else { turn_start() };
    CutPointResult {
        first_kept_entry_index: cut,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index.is_some(),
    }
}

fn nearest_cut(cut_points: &[usize], index: usize) -> usize {
    cut_points.iter().copied().find(|candidate| *candidate >= index).unwrap_or(cut_points[cut_points.len() - 1])
}

/// Walk back from the newest entry in `start..end`, summing estimates until
/// `keep_recent_tokens`, then cut at the nearest valid point at or after there.
pub fn find_cut_point(entries: &[&Entry], start: usize, end: usize, keep_recent_tokens: u64) -> CutPointResult {
    let messages: Vec<Vec<Message>> = entries.iter().map(|entry| session_entry_to_context_messages(entry)).collect();
    let cut_points: Vec<usize> = (start..end)
        .filter(|index| entries[*index].compaction().is_none() && messages[*index].iter().any(is_cut_point_message))
        .collect();
    let Some(&first) = cut_points.first() else {
        return CutPointResult { first_kept_entry_index: start, turn_start_index: None, is_split_turn: false };
    };
    let mut cut = first;
    let mut accumulated = 0;
    for index in (start..end).rev() {
        let tokens: u64 = messages[index].iter().map(estimate_tokens).sum();
        if tokens == 0 {
            continue;
        }
        accumulated += tokens;
        if accumulated >= keep_recent_tokens {
            cut = nearest_cut(&cut_points, index);
            break;
        }
    }
    while cut > start && entries[cut - 1].compaction().is_none() && messages[cut - 1].is_empty() {
        cut -= 1;
    }
    split_result(cut, is_turn_start_entry(entries[cut]), || find_turn_start_index(entries, cut, start))
}

fn is_projected_turn_start(entry: &ProjectedEntry) -> bool {
    entry.source.compaction().is_none() && entry.messages.iter().any(is_turn_start_message)
}

fn is_intrinsically_visible(entry: &ProjectedEntry) -> bool {
    entry.source.kind() != "context_edit" && !session_entry_to_context_messages(entry.source).is_empty()
}

fn is_omitted(entry: &ProjectedEntry) -> bool {
    is_intrinsically_visible(entry) && entry.messages.is_empty()
}

/// Whether `suffix` holds only an omitted recovery response, its omission edits, and metadata.
fn is_recovery_omission_suffix(suffix: &[ProjectedEntry]) -> bool {
    let omitted_ids: Vec<&str> =
        suffix.iter().filter(|entry| is_omitted(entry)).map(|entry| entry.source.id()).collect();
    let external_replacement = suffix.iter().any(|entry| match entry.source.data() {
        EntryData::ContextEdit { target_id, replacement } => {
            replacement.is_some() && !omitted_ids.contains(&target_id.as_str())
        }
        _ => false,
    });
    !external_replacement
        && suffix.iter().any(|entry| entry.source.is_message_role("assistant") && is_omitted(entry))
        && suffix
            .iter()
            .all(|entry| entry.source.compaction().is_none() && (!is_intrinsically_visible(entry) || is_omitted(entry)))
}

/// [`find_cut_point`] over the edited projection. When the budget runs out and the entries
/// after the cut are only an omitted recovery response (plus metadata), the cut moves past it.
pub fn find_projected_cut_point(
    entries: &[ProjectedEntry],
    start: usize,
    end: usize,
    keep_recent_tokens: u64,
) -> CutPointResult {
    let cut_points: Vec<usize> = (start..end)
        .filter(|index| {
            entries[*index].source.compaction().is_none() && entries[*index].messages.iter().any(is_cut_point_message)
        })
        .collect();
    let Some(&first) = cut_points.first() else {
        return CutPointResult { first_kept_entry_index: start, turn_start_index: None, is_split_turn: false };
    };
    let (mut cut, mut accumulated, mut exceeded) = (first, 0, false);
    for index in (start..end).rev() {
        let tokens: u64 = entries[index].messages.iter().map(estimate_tokens).sum();
        if tokens == 0 {
            continue;
        }
        accumulated += tokens;
        if accumulated >= keep_recent_tokens {
            exceeded = true;
            cut = nearest_cut(&cut_points, index);
            break;
        }
    }
    if exceeded && cut + 1 < end && is_recovery_omission_suffix(&entries[cut + 1..end]) {
        cut += 1;
    }
    while cut > start && entries[cut - 1].source.compaction().is_none() && entries[cut - 1].messages.is_empty() {
        cut -= 1;
    }
    let starts_turn = entries.get(cut).is_some_and(is_projected_turn_start);
    let last = cut.min(end.saturating_sub(1));
    split_result(cut, starts_turn, || (start..=last).rev().find(|index| is_projected_turn_start(&entries[*index])))
}
