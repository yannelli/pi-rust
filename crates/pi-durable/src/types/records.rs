//! Durable table records: conversations, entries, submissions, and tasks (spec section 2).
//!
//! Field order follows the order upstream builds each record, so `JSON.stringify` output matches.

use pi_ai::Message;
use pi_chord::Json;
use serde::{Deserialize, Serialize};

use crate::ids::{ConversationId, EntryId, SubmissionId, TaskId};

/// Fork source and inclusive parent entry through which history is inherited.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationParent {
    pub conversation_id: ConversationId,
    pub at: EntryId,
}

/// Creator edge used for attribution, subtree abort, and subtree idle waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationOwner {
    pub conversation_id: ConversationId,
    pub task_id: TaskId,
}

/// Immutable identity, history ancestry, and task ownership of a transcript scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRecord {
    pub id: ConversationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ConversationParent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<ConversationOwner>,
}

/// Ownership selected explicitly whenever a conversation is created.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ConversationOwnership {
    Ownerless,
    #[serde(rename_all = "camelCase")]
    Task {
        task_id: TaskId,
    },
}

/// What a context edit does to its target entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContextEditAction {
    Omit,
    Replace,
}

/// An immutable override of one visible entry's contribution to model context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEdit {
    /// Entry whose model messages are omitted or replaced.
    pub target: EntryId,
    pub action: ContextEditAction,
    /// Messages contributed instead of the target's; present exactly for `replace`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<Message>>,
}

impl ContextEdit {
    pub fn omit(target: EntryId) -> Self {
        Self { target, action: ContextEditAction::Omit, messages: None }
    }

    pub fn replace(target: EntryId, messages: Vec<Message>) -> Self {
        Self { target, action: ContextEditAction::Replace, messages: Some(messages) }
    }
}

/// Immutable transcript event with separate model-facing and application-facing payloads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryRecord {
    /// Application-defined entry discriminator.
    pub kind: String,
    /// Messages contributed to model context; absent for display or bookkeeping entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Vec<Message>>,
    /// JSON payload consumed by views, extensions, or bookkeeping logic.
    #[serde(default, deserialize_with = "super::present_json::deserialize", skip_serializing_if = "Option::is_none")]
    pub data: Option<Json>,
    /// Context-only overrides of earlier visible entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<Vec<ContextEdit>>,
    pub id: EntryId,
    pub conversation_id: ConversationId,
    /// First entry in the active context selected by this entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<EntryId>,
    /// Task that appended this entry, when durable work produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_task_id: Option<TaskId>,
}

/// Where a drafted entry starts the active context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftHead {
    /// Start active context at the newly assigned entry ID.
    SelfEntry,
    Entry(EntryId),
}

/// Entry content supplied before the Session assigns identity and task attribution.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EntryDraft {
    pub kind: String,
    pub model: Option<Vec<Message>>,
    pub data: Option<Json>,
    pub head: Option<DraftHead>,
    pub edits: Option<Vec<ContextEdit>>,
}

impl EntryDraft {
    pub fn new(kind: impl Into<String>) -> Self {
        Self { kind: kind.into(), ..Self::default() }
    }
}

/// Whether a submission asks the model (`input`) or only appends an entry (`write`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubmissionType {
    Input,
    Write,
}

/// Lifecycle status of a submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubmissionStatus {
    /// Admitted but not yet represented in the transcript.
    Queued,
    /// An input added to the transcript and owned by an active run.
    Placed,
    Done,
    Unanswered,
}

impl SubmissionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Placed => "placed",
            Self::Done => "done",
            Self::Unanswered => "unanswered",
        }
    }

    pub fn is_settled(self) -> bool {
        matches!(self, Self::Done | Self::Unanswered)
    }
}

/// Durable lifecycle of one admitted user input or passive entry write. Which optional fields are present follows
/// `type` and `status` as in upstream's union: `entry` once placed, `answer` for a done input, `reason` when unanswered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionRecord {
    pub conversation_id: ConversationId,
    #[serde(rename = "type")]
    pub kind: SubmissionType,
    /// Host-provided deduplication key, scoped to the conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub status: SubmissionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<EntryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, deserialize_with = "super::present_json::deserialize", skip_serializing_if = "Option::is_none")]
    pub detail: Option<Json>,
    pub id: SubmissionId,
}

/// Submission fields supplied before the Session assigns an ID.
#[derive(Clone, Debug, PartialEq)]
pub struct SubmissionCreate {
    pub conversation_id: ConversationId,
    pub kind: SubmissionType,
    pub request_id: Option<String>,
    pub status: SubmissionStatus,
    pub entry: Option<EntryId>,
    pub answer: Option<EntryId>,
    pub reason: Option<String>,
    pub detail: Option<Json>,
}

impl SubmissionCreate {
    /// A queued submission of `kind`.
    pub fn queued(conversation_id: ConversationId, kind: SubmissionType, request_id: Option<String>) -> Self {
        Self {
            conversation_id,
            kind,
            request_id,
            status: SubmissionStatus::Queued,
            entry: None,
            answer: None,
            reason: None,
            detail: None,
        }
    }

    pub fn into_record(self, id: SubmissionId) -> SubmissionRecord {
        SubmissionRecord {
            conversation_id: self.conversation_id,
            kind: self.kind,
            request_id: self.request_id,
            status: self.status,
            entry: self.entry,
            answer: self.answer,
            reason: self.reason,
            detail: self.detail,
            id,
        }
    }
}

/// Terminal status staged for a submission; identity, type, and entry come from its current record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum SubmissionSettlement {
    Done {
        answer: EntryId,
    },
    Unanswered {
        reason: String,
        #[serde(
            default,
            deserialize_with = "super::present_json::deserialize",
            skip_serializing_if = "Option::is_none"
        )]
        detail: Option<Json>,
    },
}
