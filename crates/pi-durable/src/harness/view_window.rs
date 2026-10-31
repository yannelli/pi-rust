//! The windows of conversation views (spec section 9.3): how a new observer's window is read, and how each window
//! takes a publication.

use std::sync::Arc;

use pi_chord::delta::{Op, Path, Seg, apply_immutable};
use pi_chord::{Json, JsonMap, WatchChange};

use super::view::ViewObserver;
use crate::documents::encode;
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, DocumentId, EntryId};
use crate::session::ObservedCommit;
use crate::types::{CommitChange, Cursor, DocumentCommitChange, EntryQuery, EntryRecord, ScanOrder, Storage};
use pi_chord::Context;

/// Which entries a view's window holds (spec section 9.3): between `min` and `max`. It starts with the newest `min`,
/// takes appends, and past `max` drops its oldest down to `min` in one splice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConversationWindow {
    /// Where the first value's window starts, inclusive, when at most `max` entries are at or after it. Later entries
    /// are appended whatever their ID; a `from` past the newest entry starts empty, with `before` the newest.
    pub from: Option<EntryId>,
    /// Default 50; at least 1.
    pub min: Option<usize>,
    /// Default `2 * min`; at least `min`.
    pub max: Option<usize>,
}

const DEFAULT_WINDOW_MIN: usize = 50;
const SCAN_PAGE_SIZE: usize = 256;
pub(super) const MOUNTED_KINDS: [&str; 5] = ["pi.agent", "pi.live", "pi.inbox", "pi.provider", "pi.usage"];

/// A window with its defaults applied.
#[derive(Clone, Copy)]
pub(super) struct WindowBounds {
    from: Option<EntryId>,
    pub min: usize,
    pub max: usize,
}

/// `window` with its defaults; an error for a `min` below 1 or a `max` below `min`.
pub(super) fn resolve_window(window: Option<ConversationWindow>) -> DurableResult<WindowBounds> {
    let window = window.unwrap_or_default();
    let min = window.min.unwrap_or(DEFAULT_WINDOW_MIN);
    if min < 1 {
        return Err(DurableError::error(format!("Window min must be an integer >= 1, got {min}")));
    }
    let max = window.max.unwrap_or(min.saturating_mul(2));
    if max < min {
        return Err(DurableError::error(format!("Window max must be an integer >= min ({min}), got {max}")));
    }
    Ok(WindowBounds { from: window.from, min, max })
}

/// One observer of a mount and its window: the bounds and the revision it was last given.
pub(super) struct Windowed {
    pub min: usize,
    pub max: usize,
    pub value: Json,
    pub observer: Arc<dyn ViewObserver>,
}

/// One conversation's mount: the documents it shows, shared by every observer, and each observer's window.
pub(super) struct Mount {
    pub conversation: Json,
    pub docs: Json,
    /// Mounted incarnation and definition version per kind; another incarnation or version is set whole.
    pub incarnations: std::collections::HashMap<String, (DocumentId, u32)>,
    pub observers: Vec<(u64, Windowed)>,
}

/// A view revision; `before` is a key only when set.
pub(super) fn view(conversation: &Json, entries: Json, before: Option<EntryId>, docs: &Json) -> Json {
    let mut value = JsonMap::new();
    value.insert("conversation", conversation.clone());
    value.insert("entries", entries);
    if let Some(before) = before {
        value.insert("before", Json::from(before.get()));
    }
    value.insert("docs", docs.clone());
    Json::object(value)
}

/// A new observer's window, read on the Session line with bounded scans: the entries from `from` when at most `max`
/// are at or after it, otherwise the newest `min`, and the entry before them.
pub(super) async fn load_window(
    storage: &dyn Storage,
    conversation_id: ConversationId,
    bounds: WindowBounds,
    context: &Context,
) -> DurableResult<(Vec<Json>, Option<EntryId>)> {
    let WindowBounds { from, min, max } = bounds;
    let encoded = |entries: Vec<EntryRecord>| entries.iter().map(encode).collect::<DurableResult<Vec<_>>>();
    if let Some(from) = from {
        let query = EntryQuery {
            min_entry_id: Some(from),
            order: Some(ScanOrder::Ascending),
            ..EntryQuery::new(conversation_id)
        };
        let since = scan_up_to(storage, &query, max.saturating_add(1), context).await?;
        if since.len() <= max {
            let before = match from.get().checked_sub(1) {
                Some(previous) => {
                    let query =
                        EntryQuery { max_entry_id: Some(EntryId(previous)), ..EntryQuery::new(conversation_id) };
                    storage.scan_entries(&query, 1, None, context).await?.items.first().map(|entry| entry.id)
                }
                None => None,
            };
            return Ok((encoded(since)?, before));
        }
    }
    let mut newest = scan_up_to(storage, &EntryQuery::new(conversation_id), min.saturating_add(1), context).await?;
    let before = if newest.len() > min { newest.pop().map(|entry| entry.id) } else { None };
    newest.reverse();
    Ok((encoded(newest)?, before))
}

/// Up to `limit` entries of `query`, following cursors past short pages.
async fn scan_up_to(
    storage: &dyn Storage,
    query: &EntryQuery,
    limit: usize,
    context: &Context,
) -> DurableResult<Vec<EntryRecord>> {
    let mut entries = Vec::new();
    let mut cursor: Option<Cursor> = None;
    loop {
        let size = SCAN_PAGE_SIZE.min(limit - entries.len());
        let page = storage.scan_entries(query, size, cursor.as_ref(), context).await?;
        entries.extend(page.items);
        cursor = page.next;
        if cursor.is_none() || entries.len() >= limit {
            return Ok(entries);
        }
    }
}

pub(super) type Delivery = (Arc<dyn ViewObserver>, WatchChange);

fn key_path(keys: &[&str]) -> Path {
    keys.iter().map(|key| Seg::Key(Arc::from(*key))).collect()
}

/// Apply one publication to the mount's documents once, then queue each window's next revision: appended entries,
/// and past its `max` its oldest dropped down to its `min` in one splice. Entry writes come in ID order.
pub(super) fn advance(
    id: ConversationId,
    mount: &mut Mount,
    publication: &ObservedCommit,
    deliveries: &mut Vec<Delivery>,
) -> DurableResult<()> {
    let mut doc_ops = Vec::new();
    let mut appended = Vec::new();
    for change in publication.changes() {
        match change {
            CommitChange::Entry(entry) if entry.conversation_id == id => appended.push(encode(entry)?),
            CommitChange::Document(DocumentCommitChange::Document { record, conversation_id, version, value, ops })
                if *conversation_id == Some(id) =>
            {
                let kind = record.kind.as_str();
                if !MOUNTED_KINDS.contains(&kind) || record.key.is_some() {
                    continue;
                }
                let path = key_path(&["docs", kind]);
                let mounted = mount.incarnations.get(kind).copied();
                match value {
                    None => {
                        if mounted.map(|(doc, _)| doc) != Some(record.id) {
                            continue;
                        }
                        mount.incarnations.remove(kind);
                        doc_ops.push(Op::Delete(path));
                    }
                    Some(value) => {
                        if mounted.is_some_and(|(doc, at)| doc == record.id && Some(at) == *version) {
                            doc_ops.extend(ops.iter().map(|op| prefixed(op, &path)));
                        } else {
                            mount.incarnations.insert(kind.to_owned(), (record.id, version.unwrap_or(1)));
                            doc_ops.push(Op::Set(path, value.clone()));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if doc_ops.is_empty() && appended.is_empty() {
        return Ok(());
    }
    if !doc_ops.is_empty() {
        let mut root = JsonMap::new();
        root.insert("docs", mount.docs.clone());
        let applied = apply_immutable(Some(&Json::object(root)), &doc_ops)?;
        mount.docs = applied.get("docs").cloned().unwrap_or_else(Json::empty_object);
    }
    for (_, windowed) in &mut mount.observers {
        let mut entries = windowed.value.get("entries").cloned().unwrap_or_else(|| Json::array(Vec::new()));
        let mut before = windowed.value.get("before").and_then(Json::as_u64).map(EntryId);
        let mut ops = doc_ops.clone();
        if !appended.is_empty() {
            let mut kept = entries.as_array().cloned().unwrap_or_default();
            ops.push(Op::Splice(key_path(&["entries"]), kept.len() as u64, 0, appended.clone()));
            kept.extend(appended.iter().cloned());
            if kept.len() > windowed.max {
                let drop = kept.len() - windowed.min;
                before = kept[drop - 1].get("id").and_then(Json::as_u64).map(EntryId);
                kept.drain(..drop);
                ops.push(Op::Splice(key_path(&["entries"]), 0, drop as u64, Vec::new()));
                if let Some(before) = before {
                    ops.push(Op::Set(key_path(&["before"]), Json::from(before.get())));
                }
            }
            entries = Json::array(kept);
        }
        windowed.value = view(&mount.conversation, entries, before, &mount.docs);
        deliveries.push((windowed.observer.clone(), WatchChange::new(windowed.value.clone(), ops)));
    }
    Ok(())
}

/// `op` moved under `prefix`; a root replacement becomes a set of the prefix.
fn prefixed(op: &Op, prefix: &Path) -> Op {
    let at = |path: &Path| -> Path { prefix.iter().chain(path.iter()).cloned().collect() };
    match op {
        Op::Replace(value) => Op::Set(prefix.clone(), value.clone()),
        Op::Set(path, value) => Op::Set(at(path), value.clone()),
        Op::Delete(path) => Op::Delete(at(path)),
        Op::Append(path, text) => Op::Append(at(path), text.clone()),
        Op::Truncate(path, count) => Op::Truncate(at(path), *count),
        Op::Splice(path, index, remove, items) => Op::Splice(at(path), *index, *remove, items.clone()),
        Op::Move(path, permutation) => Op::Move(at(path), permutation.clone()),
    }
}
