//! Scope markers of typed tokens. Each fixes the owner argument of every access and the persisted semantics.

use super::DocSemantics;
use crate::ids::{ConversationId, TaskId};
use crate::types::{DocumentFork, DocumentHistory, DocumentScope};

/// Owner type and semantics of a typed token. Implemented by [`SessionScope`], [`LatestConversation`],
/// [`RewindableConversation`], and [`TaskScope`].
pub trait DocScope: Clone + Send + Sync + 'static {
    /// `()` for Session documents, a `ConversationId`, or a `TaskId`.
    type Owner: Copy + Send + Sync + 'static;
    fn semantics(&self) -> DocSemantics;
    fn scope_of(owner: Self::Owner) -> DocumentScope;
}

/// Scopes whose owner is a conversation (upstream `ConversationDocToken`).
pub trait ConversationDocScope: DocScope<Owner = ConversationId> {}

/// Conversation scopes that keep addressable history for as-of reads.
pub trait RewindableDocScope: ConversationDocScope {}

/// A Session-owned, current-only document.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionScope;

/// Fork policy of a latest-only conversation document; `asOf` needs rewindable history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatestFork {
    Current,
    Initial,
}

/// A conversation document that keeps only its current state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatestConversation(pub LatestFork);

/// A conversation document that keeps history for `snapshot_as_of` and `asOf` forks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RewindableConversation(pub DocumentFork);

/// A task-owned, current-only document retired with its task.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TaskScope;

impl DocScope for SessionScope {
    type Owner = ();
    fn semantics(&self) -> DocSemantics {
        DocSemantics::Session
    }
    fn scope_of((): ()) -> DocumentScope {
        DocumentScope::Session
    }
}

impl DocScope for LatestConversation {
    type Owner = ConversationId;
    fn semantics(&self) -> DocSemantics {
        let fork = match self.0 {
            LatestFork::Current => DocumentFork::Current,
            LatestFork::Initial => DocumentFork::Initial,
        };
        DocSemantics::Conversation { history: DocumentHistory::Latest, fork }
    }
    fn scope_of(conversation_id: ConversationId) -> DocumentScope {
        DocumentScope::Conversation { conversation_id }
    }
}
impl ConversationDocScope for LatestConversation {}

impl DocScope for RewindableConversation {
    type Owner = ConversationId;
    fn semantics(&self) -> DocSemantics {
        DocSemantics::Conversation { history: DocumentHistory::Rewindable, fork: self.0 }
    }
    fn scope_of(conversation_id: ConversationId) -> DocumentScope {
        DocumentScope::Conversation { conversation_id }
    }
}
impl ConversationDocScope for RewindableConversation {}
impl RewindableDocScope for RewindableConversation {}

impl DocScope for TaskScope {
    type Owner = TaskId;
    fn semantics(&self) -> DocSemantics {
        DocSemantics::Task
    }
    fn scope_of(task_id: TaskId) -> DocumentScope {
        DocumentScope::Task { task_id }
    }
}
