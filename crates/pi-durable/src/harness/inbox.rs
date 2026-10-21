//! The `pi.inbox` queue and boundaries (upstream `harness/inbox.ts`, spec section 6).

use std::sync::LazyLock;

use pi_ai::{Message, UserMessage};
use pi_chord::{Json, JsonMap};
use serde::{Deserialize, Serialize};

use super::settings::Settings;
use super::types::{QueueMode, UserInput};
use crate::documents::{DocSpec, DocToken, LatestConversation, LatestFork, define_doc, encode};
use crate::entries::{TypedEntryDraft, USER_ENTRY};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, EntryId, SubmissionId};
use crate::session::{Draft, Tx};
use crate::types::{DraftHead, EntryDraft, SubmissionSettlement};

/// A queued submission: user input for a run, or a passive entry write.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum InboxItem {
    #[serde(rename = "steer")]
    Steer { id: SubmissionId, content: UserInput },
    #[serde(rename = "followUp")]
    FollowUp { id: SubmissionId, content: UserInput },
    /// `entry` is an `EntryDraft` stored as plain JSON.
    #[serde(rename = "write")]
    Write {
        id: SubmissionId,
        #[serde(with = "crate::harness::util::json_map")]
        entry: JsonMap,
    },
}

impl InboxItem {
    pub fn id(&self) -> SubmissionId {
        match self {
            Self::Steer { id, .. } | Self::FollowUp { id, .. } | Self::Write { id, .. } => *id,
        }
    }

    pub fn mode(&self) -> &'static str {
        match self {
            Self::Steer { .. } => "steer",
            Self::FollowUp { .. } => "followUp",
            Self::Write { .. } => "write",
        }
    }
}

/// Built-in queue of one conversation's submissions waiting for a boundary, in ID order.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InboxState {
    pub items: Vec<InboxItem>,
}

pub static INBOX_DOC: LazyLock<DocToken<InboxState, LatestConversation>> = LazyLock::new(|| {
    let spec =
        DocSpec::new("pi.inbox", 1, LatestConversation(LatestFork::Initial), InboxState::default, |value, _, _| {
            Ok(value.get("items").and_then(Json::as_array).is_none_or(Vec::is_empty))
        });
    define_doc(spec).unwrap_or_else(|_| unreachable!("pi.inbox is a valid definition"))
});

/// An entry draft as the plain JSON object an inbox item stores.
pub fn entry_draft_json(draft: &EntryDraft) -> DurableResult<JsonMap> {
    let mut map = JsonMap::new();
    map.insert("kind", draft.kind.as_str());
    if let Some(model) = &draft.model {
        map.insert("model", encode(model)?);
    }
    if let Some(data) = &draft.data {
        map.insert("data", data.clone());
    }
    match draft.head {
        Some(DraftHead::SelfEntry) => {
            map.insert("head", "self");
        }
        Some(DraftHead::Entry(id)) => {
            map.insert("head", encode(&id)?);
        }
        None => {}
    }
    if let Some(edits) = &draft.edits {
        map.insert("edits", encode(edits)?);
    }
    Ok(map)
}

/// The entry draft an inbox item stores.
pub fn entry_draft_from_json(map: &JsonMap) -> DurableResult<EntryDraft> {
    let kind =
        map.get("kind").and_then(Json::as_str).ok_or_else(|| DurableError::type_error("Entry draft without kind"))?;
    let head = match map.get("head") {
        None => None,
        Some(head) if head.as_str() == Some("self") => Some(DraftHead::SelfEntry),
        Some(head) => Some(DraftHead::Entry(head.deserialize_into::<EntryId>()?)),
    };
    Ok(EntryDraft {
        kind: kind.to_owned(),
        model: map.get("model").map(Json::deserialize_into).transpose()?,
        data: map.get("data").cloned(),
        head,
        edits: map.get("edits").map(Json::deserialize_into).transpose()?,
    })
}

/// What a boundary reads before the commit's first table write, and the newest head it has seen so far.
pub struct Boundary {
    pub conversation_id: ConversationId,
    pub inbox: Draft<InboxState>,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    /// Start of the active range; advanced by heads written in this commit.
    pub head: Option<EntryId>,
}

/// Selected user items, in ID order, and whether a `head: "self"` write (a reset) was placed.
pub struct BoundaryResult {
    pub users: Vec<SubmissionId>,
    pub reset: bool,
}

/// Where a boundary sits: after a tool round, or at a final answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryAt {
    PostTools,
    Final,
}

/// Read what a boundary needs. Table reads come before the commit's first table write.
pub async fn prepare_boundary(
    tx: &Tx,
    conversation_id: ConversationId,
    settings: &Settings,
) -> DurableResult<Boundary> {
    let head = tx.latest_head_marker(conversation_id).await?.and_then(|marker| marker.head);
    let inbox = tx.doc(&INBOX_DOC, conversation_id).await?;
    Ok(Boundary {
        conversation_id,
        inbox,
        steering_mode: settings.steering_mode,
        follow_up_mode: settings.follow_up_mode,
        head,
    })
}

fn is_reset(item: &InboxItem) -> bool {
    matches!(item, InboxItem::Write { entry, .. } if entry.get("head").and_then(Json::as_str) == Some("self"))
}

/// Place the queued items a boundary selects: every write, the first or all steers, and at `Final` the first or
/// all follow-ups. A selected reset makes a post-tools boundary final. Writes go first and user items after them.
pub async fn apply_boundary(
    tx: &Tx,
    boundary: &mut Boundary,
    at: BoundaryAt,
    now: f64,
) -> DurableResult<BoundaryResult> {
    let items = boundary.inbox.get()?.items;
    let reset = items.iter().any(is_reset);
    let last = at == BoundaryAt::Final || reset;
    let pick = |steer: bool, mode: QueueMode| -> Vec<usize> {
        let indexes = items.iter().enumerate().filter(|(_, item)| match item {
            InboxItem::Steer { .. } => steer,
            InboxItem::FollowUp { .. } => !steer,
            InboxItem::Write { .. } => false,
        });
        let indexes: Vec<usize> = indexes.map(|(index, _)| index).collect();
        if mode == QueueMode::All { indexes } else { indexes.into_iter().take(1).collect() }
    };
    let writes: Vec<usize> =
        items.iter().enumerate().filter(|(_, item)| matches!(item, InboxItem::Write { .. })).map(|(i, _)| i).collect();
    let mut users = pick(true, boundary.steering_mode);
    if last {
        users.extend(pick(false, boundary.follow_up_mode));
    }
    users.sort_unstable();
    for &index in &writes {
        let InboxItem::Write { id, entry } = &items[index] else { continue };
        let draft = entry_draft_from_json(entry)?;
        if is_stale(boundary, &draft) {
            let settlement = SubmissionSettlement::Unanswered { reason: "stale".into(), detail: None };
            tx.settle_submission(*id, settlement)?;
            continue;
        }
        let head = draft.head;
        let appended = tx.append_entry(boundary.conversation_id, draft).await?;
        match head {
            Some(DraftHead::SelfEntry) => boundary.head = Some(appended.id),
            Some(DraftHead::Entry(target)) => boundary.head = Some(target),
            None => {}
        }
        tx.place_submission(*id, appended.id)?;
    }
    let mut placed = Vec::new();
    for &index in &users {
        let (InboxItem::Steer { id, content } | InboxItem::FollowUp { id, content }) = &items[index] else { continue };
        let entry = append_user(tx, boundary.conversation_id, content.clone(), now).await?;
        tx.place_submission(*id, entry)?;
        placed.push(*id);
    }
    let mut removed: Vec<usize> = writes.into_iter().chain(users).collect();
    removed.sort_unstable_by(|a, b| b.cmp(a));
    boundary.inbox.edit(|state| {
        for index in removed {
            state.items.remove(index);
        }
    })?;
    Ok(BoundaryResult { users: placed, reset })
}

/// Append a `pi.user` entry with one user message.
pub async fn append_user(
    tx: &Tx,
    conversation_id: ConversationId,
    content: UserInput,
    now: f64,
) -> DurableResult<EntryId> {
    let message = Message::User(UserMessage { content, timestamp: now as i64 });
    let draft = TypedEntryDraft { model: Some(vec![message]), ..TypedEntryDraft::new(()) };
    Ok(tx.append_typed_entry(&USER_ENTRY, conversation_id, draft).await?.record.id)
}

/// Whether a head write targets an entry before the active range, so placing it would bring back cut history.
pub fn is_stale(boundary: &Boundary, entry: &EntryDraft) -> bool {
    matches!((entry.head, boundary.head), (Some(DraftHead::Entry(target)), Some(head)) if target < head)
}

/// Remove a withdrawn submission's item; the caller settles the submission.
pub async fn remove_inbox_item(tx: &Tx, conversation_id: ConversationId, id: SubmissionId) -> DurableResult<()> {
    let inbox = tx.doc(&INBOX_DOC, conversation_id).await?;
    inbox.edit(|state| {
        if let Some(index) = state.items.iter().position(|item| item.id() == id) {
            state.items.remove(index);
        }
    })
}

/// Withdraw every queued input of a conversation: each settles `unanswered` with `reason`, `aborted` for an abort;
/// writes stay queued.
pub async fn withdraw_queued_inputs(tx: &Tx, conversation_id: ConversationId, reason: &str) -> DurableResult<()> {
    let inbox = tx.doc(&INBOX_DOC, conversation_id).await?;
    let items = inbox.get()?.items;
    let withdrawn: Vec<SubmissionId> =
        items.iter().filter(|item| !matches!(item, InboxItem::Write { .. })).map(InboxItem::id).collect();
    if withdrawn.is_empty() {
        return Ok(());
    }
    for id in withdrawn.iter().rev() {
        tx.settle_submission(*id, SubmissionSettlement::Unanswered { reason: reason.into(), detail: None })?;
    }
    inbox.edit(|state| state.items.retain(|item| matches!(item, InboxItem::Write { .. })))
}
