//! Model context derivation (upstream `harness/context.ts`): the active range from the newest head marker, context
//! edits, excluded stop reasons, and tool result ordering.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use pi_ai::{ContentBlock, Message, StopReason, ToolCall, ToolResultMessage};
use pi_chord::Context;

use super::types::ContextView;
use super::util::scan_all;
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, EntryId};
use crate::session::Session;
use crate::types::{ContextEdit, ContextEditAction, EntryQuery, EntryRecord, ScanOrder, Storage};

const SCAN_PAGE_SIZE: usize = 256;
const MISSING_RESULT_TEXT: &str = "Tool result unavailable: history ends before this call completed.";

/// Head marker and newest visible entry that fix one committed context range.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextBounds {
    pub head: Option<EntryRecord>,
    pub tail: EntryId,
}

/// The bounds of the current context, or of the context cut off at the visible entry `at`. Run on the line.
pub async fn capture_context_bounds(
    storage: &dyn Storage,
    conversation_id: ConversationId,
    context: &Context,
    at: Option<EntryId>,
) -> DurableResult<Option<ContextBounds>> {
    let tail = match at {
        None => {
            let page = storage.scan_entries(&EntryQuery::new(conversation_id), 1, None, context).await?;
            match page.items.first() {
                Some(entry) => entry.id,
                None => return Ok(None),
            }
        }
        Some(at) => {
            if storage.visible_entry(conversation_id, at, context).await?.is_none() {
                let message = format!("Entry {at} is not visible from conversation {conversation_id}");
                return Err(DurableError::error(message));
            }
            at
        }
    };
    let head = storage.find_latest_head_marker(conversation_id, Some(tail), context).await?;
    Ok(Some(ContextBounds { head, tail }))
}

/// One context read: the visible entries it scanned, oldest first, and the view derived from them.
#[derive(Debug)]
pub struct ContextRange {
    pub bounds: ContextBounds,
    pub entries: Vec<EntryRecord>,
    pub view: ContextView,
    /// Targets of the edits in `entries`.
    pub edited: HashSet<EntryId>,
    /// `view.messages` from the contributed messages before the last assistant message.
    pub settled: Vec<Message>,
    /// Contributed messages from the last assistant message on, before tool result ordering.
    pub open: Vec<Message>,
}

/// A context read that reuses an earlier range of the same conversation: with the same head marker, only entries
/// after its tail are scanned. Returns the view and the range for the next read.
pub async fn read_context_from(
    session: &Session,
    conversation_id: ConversationId,
    context: &Context,
    at: Option<EntryId>,
    earlier: Option<Arc<ContextRange>>,
) -> DurableResult<(ContextView, Option<Arc<ContextRange>>)> {
    let storage = session.storage();
    let bounds = {
        let (storage, context) = (storage.clone(), context.clone());
        session.read_on_line(async move { capture_context_bounds(&*storage, conversation_id, &context, at).await })
    }
    .await?;
    let Some(bounds) = bounds else { return Ok((ContextView::default(), None)) };
    let head_id = |b: &ContextBounds| b.head.as_ref().map(|h| h.id);
    let range = match earlier {
        Some(earlier) if head_id(&earlier.bounds) == head_id(&bounds) => {
            if bounds.tail == earlier.bounds.tail {
                earlier
            } else if bounds.tail < earlier.bounds.tail {
                let entries = earlier.entries.iter().filter(|e| e.id <= bounds.tail).cloned().collect();
                Arc::new(derive_range(bounds, entries))
            } else {
                let mut query = EntryQuery::new(conversation_id);
                query.min_entry_id = Some(EntryId(earlier.bounds.tail.0 + 1));
                query.max_entry_id = Some(bounds.tail);
                let added = scan_range(&*storage, query, context).await?;
                Arc::new(extend_range(&earlier, bounds, added))
            }
        }
        _ => {
            let entries = scan_range(&*storage, range_query(conversation_id, &bounds), context).await?;
            Arc::new(derive_range(bounds, entries))
        }
    };
    Ok((range.view.clone(), Some(range)))
}

/// Committed context of one conversation: bounds captured on the Session line, entries derived off it.
pub async fn read_context(
    session: &Session,
    conversation_id: ConversationId,
    context: &Context,
    at: Option<EntryId>,
) -> DurableResult<ContextView> {
    Ok(read_context_from(session, conversation_id, context, at, None).await?.0)
}

fn derive_range(bounds: ContextBounds, entries: Vec<EntryRecord>) -> ContextRange {
    let mut edits: HashMap<EntryId, &ContextEdit> = HashMap::new();
    for entry in &entries {
        for edit in entry.edits.iter().flatten() {
            edits.insert(edit.target, edit);
        }
    }
    let active = select_active(bounds.head.as_ref(), &entries);
    let contributions: Vec<Vec<Message>> =
        active.iter().map(|entry| contribute(entry, edits.get(&entry.id).copied())).collect();
    let flat: Vec<Message> = contributions.iter().flatten().cloned().collect();
    let (settled, open) = settle(Vec::new(), flat);
    let mut messages = settled.clone();
    messages.extend(order_tool_results(&open));
    let messages = lead_with_system(messages);
    let view = ContextView { head: bounds.head.clone(), entries: active, contributions, messages };
    let edited = edits.keys().copied().collect();
    ContextRange { bounds, entries, view, edited, settled, open }
}

fn extend_range(earlier: &ContextRange, bounds: ContextBounds, added: Vec<EntryRecord>) -> ContextRange {
    let mut entries = earlier.entries.clone();
    entries.extend(added.iter().cloned());
    if added.iter().any(|e| e.edits.is_some() || e.head.is_some() || earlier.edited.contains(&e.id)) {
        return derive_range(bounds, entries);
    }
    let contributions: Vec<Vec<Message>> = added.iter().map(|entry| contribute(entry, None)).collect();
    let mut open = earlier.open.clone();
    open.extend(contributions.iter().flatten().cloned());
    let (settled, open) = settle(earlier.settled.clone(), open);
    let mut messages = settled.clone();
    messages.extend(order_tool_results(&open));
    let mut view_entries = earlier.view.entries.clone();
    view_entries.extend(added);
    let mut view_contributions = earlier.view.contributions.clone();
    view_contributions.extend(contributions);
    let view = ContextView {
        head: bounds.head.clone(),
        entries: view_entries,
        contributions: view_contributions,
        messages: lead_with_system(messages),
    };
    ContextRange { bounds, entries, view, edited: earlier.edited.clone(), settled, open }
}

/// Move a system message that only user messages precede to the front.
fn lead_with_system(mut messages: Vec<Message>) -> Vec<Message> {
    let Some(index) = messages.iter().position(|m| !matches!(m, Message::User(_))) else { return messages };
    if index == 0 || !matches!(messages[index], Message::System(_)) {
        return messages;
    }
    let system = messages.remove(index);
    messages.insert(0, system);
    messages
}

fn excluded(message: &Message) -> bool {
    let Message::Assistant(assistant) = message else { return false };
    matches!(assistant.stop_reason, StopReason::Aborted | StopReason::Error | StopReason::Deferred)
}

/// One active entry's model messages after its edit and excluded stop reasons.
fn contribute(entry: &EntryRecord, edit: Option<&ContextEdit>) -> Vec<Message> {
    let contributed = match edit {
        Some(edit) if edit.action == ContextEditAction::Omit => return Vec::new(),
        Some(edit) if edit.action == ContextEditAction::Replace => edit.messages.clone().unwrap_or_default(),
        _ => entry.model.clone().unwrap_or_default(),
    };
    contributed.into_iter().filter(|message| !excluded(message)).collect()
}

/// Move the ordered messages before the last assistant message of `open` to `settled`.
fn settle(mut settled: Vec<Message>, open: Vec<Message>) -> (Vec<Message>, Vec<Message>) {
    let last = open.iter().rposition(|m| matches!(m, Message::Assistant(_)));
    match last {
        Some(last) if last > 0 => {
            settled.extend(order_tool_results(&open[..last]));
            (settled, open[last..].to_vec())
        }
        _ => (settled, open),
    }
}

/// The raw active entries within captured bounds, without deriving model context.
pub async fn active_entries(
    storage: &dyn Storage,
    conversation_id: ConversationId,
    bounds: Option<&ContextBounds>,
    context: &Context,
) -> DurableResult<Vec<EntryRecord>> {
    let Some(bounds) = bounds else { return Ok(Vec::new()) };
    let entries = scan_range(storage, range_query(conversation_id, bounds), context).await?;
    Ok(select_active(bounds.head.as_ref(), &entries))
}

fn range_query(conversation_id: ConversationId, bounds: &ContextBounds) -> EntryQuery {
    let mut query = EntryQuery::new(conversation_id);
    query.max_entry_id = Some(bounds.tail);
    query.min_entry_id = bounds.head.as_ref().and_then(|head| head.head);
    query
}

/// Visible entries within `query`, oldest first.
async fn scan_range(
    storage: &dyn Storage,
    mut query: EntryQuery,
    context: &Context,
) -> DurableResult<Vec<EntryRecord>> {
    query.order = Some(ScanOrder::Ascending);
    let query = &query;
    scan_all(|cursor| async move { storage.scan_entries(query, SCAN_PAGE_SIZE, cursor.as_ref(), context).await }).await
}

fn select_active(head: Option<&EntryRecord>, range: &[EntryRecord]) -> Vec<EntryRecord> {
    match head {
        None => range.to_vec(),
        Some(head) => std::iter::once(head.clone()).chain(range.iter().filter(|e| e.head.is_none()).cloned()).collect(),
    }
}

/// Place each assistant's tool results directly after it in call order; a missing result is synthesized and
/// unmatched results are dropped.
pub fn order_tool_results(messages: &[Message]) -> Vec<Message> {
    let mut ordered = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if matches!(message, Message::ToolResult(_)) {
            continue;
        }
        ordered.push(message.clone());
        let Message::Assistant(assistant) = message else { continue };
        let calls: Vec<&ToolCall> = assistant.tool_calls().collect();
        if calls.is_empty() {
            continue;
        }
        let mut results: HashMap<&str, usize> = HashMap::new();
        for (next, candidate) in messages.iter().enumerate().skip(index + 1) {
            match candidate {
                Message::Assistant(_) => break,
                Message::ToolResult(result) => {
                    results.entry(result.tool_call_id.as_str()).or_insert(next);
                }
                _ => {}
            }
        }
        for call in calls {
            match results.get(call.id.as_str()) {
                Some(&at) => ordered.push(messages[at].clone()),
                None => ordered.push(missing_result(call, assistant.timestamp)),
            }
        }
    }
    ordered
}

fn missing_result(call: &ToolCall, timestamp: i64) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: vec![ContentBlock::text(MISSING_RESULT_TEXT)],
        details: Some(serde_json::json!({ "reason": "missing_result" })),
        is_error: true,
        timestamp,
        ..ToolResultMessage::default()
    })
}
