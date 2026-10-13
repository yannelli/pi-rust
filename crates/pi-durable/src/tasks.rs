//! The part of a task definition the Session needs to create a task record. The Harness implements
//! [`TaskTemplate`] for its task definitions.

use pi_chord::Json;

use crate::errors::DurableResult;
use crate::ids::{ConversationId, TaskId};

/// Name, version, and first checkpoint of a task kind.
pub trait TaskTemplate: Send + Sync {
    /// Registered task definition name, stored as `TaskRecord.kind`.
    fn name(&self) -> &str;
    /// Definition version stored with the record.
    fn version(&self) -> u32;
    /// The first checkpoint for `input`: a JSON object with a string `phase`.
    fn initial(&self, input: &Json) -> DurableResult<Json>;
}

/// Who owns a new task (spec section 5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskOwnership {
    /// Its conversation owns it.
    Conversation,
    /// A live task owns it; the child lives in its owner's conversation.
    Task(TaskId),
}

/// Options of `Tx::create_task`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskOptions {
    pub ownership: TaskOwnership,
    /// Default: the owner task's conversation, or the transaction's bound conversation.
    pub conversation_id: Option<ConversationId>,
    /// Conversation-owned tasks only: excluded from ordinary idle waits, conversation aborts, and cascades.
    pub background: bool,
    /// Abort-mark the task with reason `restart` when a later Harness starts scheduling and it is still live.
    pub abandon_on_restart: bool,
}

impl TaskOptions {
    /// A conversation-owned task in `conversation_id`.
    pub fn conversation(conversation_id: ConversationId) -> Self {
        Self {
            ownership: TaskOwnership::Conversation,
            conversation_id: Some(conversation_id),
            background: false,
            abandon_on_restart: false,
        }
    }

    /// A child task owned by `owner`.
    pub fn owned_by(owner: TaskId) -> Self {
        Self {
            ownership: TaskOwnership::Task(owner),
            conversation_id: None,
            background: false,
            abandon_on_restart: false,
        }
    }
}
