//! Walks of the ownership tree: owner tasks and conversations, cascades, idle scopes, and owned live work.

use std::collections::HashMap;

use super::{SchedState, Scheduler, cancellation_intent};
use crate::errors::DurableResult;
use crate::harness::util::{lock, scan_all};
use crate::ids::{ConversationId, TaskId};
use crate::session::Tx;
use crate::types::{SubmissionQuery, SubmissionStatus, TaskRecord};

/// The immutable ownership fields of a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TaskNode {
    pub conversation_id: ConversationId,
    pub owner: Option<TaskId>,
    pub background: bool,
}

impl TaskNode {
    pub fn of(record: &TaskRecord) -> Self {
        Self { conversation_id: record.conversation_id, owner: record.owner, background: record.background }
    }
}

/// Where a walk up the ownership tree continues: an owner task, or a conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Up {
    Task(TaskId),
    Conversation(ConversationId),
}

impl Up {
    /// The parent of a task: its owner task, or its conversation.
    pub fn of(record: &TaskRecord) -> Self {
        Self::of_node(&TaskNode::of(record))
    }

    pub fn of_node(node: &TaskNode) -> Self {
        match node.owner {
            Some(owner) => Self::Task(owner),
            None => Self::Conversation(node.conversation_id),
        }
    }
}

/// One step of a walk up.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Step {
    Task(TaskId, TaskNode),
    Conversation(ConversationId),
    Unknown,
}

/// Candidate records a commit staged; they override committed records in ownership walks.
#[derive(Default)]
pub(crate) struct Overlay {
    pub tasks: HashMap<TaskId, TaskRecord>,
    pub edges: HashMap<ConversationId, Option<TaskId>>,
}

impl Overlay {
    pub fn of(tx: &Tx) -> Self {
        let tasks = tx.staged_tasks().into_iter().map(|record| (record.id, record)).collect();
        let edges = tx.staged_conversations().into_iter().map(|c| (c.id, c.owner.map(|o| o.task_id))).collect();
        Self { tasks, edges }
    }
}

/// Where ordinary ownership traversal starts: one conversation, or every ownerless conversation.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Scope {
    Conversation(ConversationId),
    Roots,
}

impl SchedState {
    pub fn set_edge(&mut self, conversation_id: ConversationId, owner: Option<TaskId>) {
        self.edges.insert(conversation_id, owner);
        if let Some(owner) = owner {
            self.conversation_owners.insert(owner);
        }
    }

    /// Owner task of a conversation: `Some(None)` when ownerless, `None` while not loaded.
    pub fn edge(&self, id: ConversationId, overlay: Option<&Overlay>) -> Option<Option<TaskId>> {
        if let Some(edge) = overlay.and_then(|overlay| overlay.edges.get(&id)) {
            return Some(*edge);
        }
        self.edges.get(&id).copied()
    }

    pub fn node(&self, id: TaskId, overlay: Option<&Overlay>) -> Option<TaskNode> {
        overlay
            .and_then(|overlay| overlay.tasks.get(&id))
            .or_else(|| self.live.get(&id))
            .map(TaskNode::of)
            .or_else(|| self.settled.get(&id).copied())
    }

    /// Walk up from `start`: owner tasks and conversations, ending at an ownerless root or an unloaded edge.
    pub fn above(&self, start: Up, overlay: Option<&Overlay>) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut at = Some(start);
        while let Some(current) = at {
            match current {
                Up::Task(id) => {
                    let Some(node) = self.node(id, overlay) else {
                        steps.push(Step::Unknown);
                        break;
                    };
                    steps.push(Step::Task(id, node));
                    at = Some(Up::of_node(&node));
                }
                Up::Conversation(id) => {
                    steps.push(Step::Conversation(id));
                    match self.edge(id, overlay) {
                        None => {
                            steps.push(Step::Unknown);
                            break;
                        }
                        Some(edge) => at = edge.map(Up::Task),
                    }
                }
            }
        }
        steps
    }

    /// Whether every owner above `start` is loaded.
    pub fn chain_known(&self, start: Up, overlay: Option<&Overlay>) -> bool {
        !self.above(start, overlay).iter().any(|step| matches!(step, Step::Unknown))
    }

    /// Live records, with the overlay's candidates replacing committed ones; terminal candidates are gone.
    pub fn live_records(&self, overlay: Option<&Overlay>) -> Vec<TaskRecord> {
        let mut records = Vec::new();
        for record in self.live.values() {
            let candidate = overlay.and_then(|overlay| overlay.tasks.get(&record.id)).unwrap_or(record);
            if !candidate.is_terminal() {
                records.push(candidate.clone());
            }
        }
        if let Some(overlay) = overlay {
            let mut added: Vec<&TaskRecord> =
                overlay.tasks.values().filter(|r| !self.live.contains_key(&r.id) && !r.is_terminal()).collect();
            added.sort_by_key(|record| record.id);
            records.extend(added.into_iter().cloned());
        }
        records
    }

    /// Every task with live ordinary owned work, mapped to that work: each live non-background task counts for
    /// every owner task above it up to and including the first background one.
    pub fn owned_live(&self, overlay: Option<&Overlay>) -> HashMap<TaskId, Vec<TaskId>> {
        let mut owned: HashMap<TaskId, Vec<TaskId>> = HashMap::new();
        for record in self.live_records(overlay) {
            if record.background {
                continue;
            }
            for step in self.above(Up::of(&record), overlay) {
                match step {
                    Step::Unknown => break,
                    Step::Conversation(_) => {}
                    Step::Task(id, node) => {
                        owned.entry(id).or_default().push(record.id);
                        if node.background {
                            break;
                        }
                    }
                }
            }
        }
        owned
    }

    /// Whether ordinary traversal from `scope` reaches `start`; `None` while an edge is not loaded.
    pub fn in_scope(&self, start: Up, scope: Scope, cross_background: bool) -> Option<bool> {
        for step in self.above(start, None) {
            match step {
                Step::Unknown => return None,
                Step::Conversation(id) => {
                    if matches!(scope, Scope::Conversation(target) if target == id) {
                        return Some(true);
                    }
                }
                Step::Task(_, node) => {
                    if node.background && !cross_background {
                        return Some(false);
                    }
                }
            }
        }
        Some(matches!(scope, Scope::Roots))
    }

    /// Whether a live owner's cancellation intent reaches `start`.
    pub fn below_cancelled(&self, start: Up) -> bool {
        self.cancelling_owner(start).is_some()
    }

    /// The nearest live owner above `start` whose cancellation intent reaches it, if any.
    pub fn cancelling_owner(&self, start: Up) -> Option<TaskRecord> {
        for step in self.above(start, None) {
            match step {
                Step::Unknown => return None,
                Step::Conversation(_) => {}
                Step::Task(id, node) => {
                    if let Some(live) = self.live.get(&id)
                        && cancellation_intent(live)
                    {
                        return Some(live.clone());
                    }
                    if node.background {
                        return None;
                    }
                }
            }
        }
        None
    }

    /// No live non-background task in the scope; a task whose owner edges are not loaded yet counts as inside.
    pub fn idle(&self, conversation_id: Option<ConversationId>) -> bool {
        let scope = conversation_id.map_or(Scope::Roots, Scope::Conversation);
        !self
            .live
            .values()
            .any(|record| !record.background && self.in_scope(Up::of(record), scope, false) != Some(false))
    }
}

impl Scheduler {
    /// Load the owner chains of every live task and, with `queued`, of every conversation with queued submissions;
    /// returns the latter. Reads committed Storage directly, so it may run inside a commit callback.
    pub(crate) async fn load_scopes(&self, queued: bool) -> DurableResult<Vec<ConversationId>> {
        let starts: Vec<Up> = {
            let state = lock(&self.state);
            state.live.values().map(Up::of).filter(|up| !state.chain_known(*up, None)).collect()
        };
        for start in starts {
            self.load_chain(start, None).await?;
        }
        if !queued {
            return Ok(Vec::new());
        }
        let storage = self.core.session.storage();
        let query = SubmissionQuery { status: Some(SubmissionStatus::Queued), ..SubmissionQuery::default() };
        let (storage_ref, query, context) = (&storage, &query, &self.context);
        let submissions = scan_all(|cursor| async move {
            storage_ref.scan_submissions(query, super::SCAN_PAGE_SIZE, cursor.as_ref(), context).await
        })
        .await?;
        let mut conversations: Vec<ConversationId> = Vec::new();
        for submission in submissions {
            if !conversations.contains(&submission.conversation_id) {
                conversations.push(submission.conversation_id);
            }
        }
        for id in &conversations {
            self.load_chain(Up::Conversation(*id), None).await?;
        }
        Ok(conversations)
    }

    /// Load the owner edges and task nodes from `start` up to its ownerless root.
    pub(crate) async fn load_chain(&self, start: Up, overlay: Option<&Overlay>) -> DurableResult<()> {
        let storage = self.core.session.storage();
        let mut at = Some(start);
        while let Some(current) = at {
            match current {
                Up::Task(id) => {
                    let known = lock(&self.state).node(id, overlay);
                    let node = match known {
                        Some(node) => node,
                        None => {
                            let Some(record) = storage.task(id, &self.context).await? else { return Ok(()) };
                            let node = TaskNode::of(&record);
                            if record.is_terminal() {
                                lock(&self.state).settled.insert(record.id, node);
                            }
                            node
                        }
                    };
                    at = Some(Up::of_node(&node));
                }
                Up::Conversation(id) => {
                    let known = lock(&self.state).edge(id, overlay);
                    let edge = match known {
                        Some(edge) => edge,
                        None => {
                            let record = storage.conversation(id, &self.context).await?;
                            let edge = record.and_then(|record| record.owner).map(|owner| owner.task_id);
                            lock(&self.state).set_edge(id, edge);
                            edge
                        }
                    };
                    at = edge.map(Up::Task);
                }
            }
        }
        Ok(())
    }
}
