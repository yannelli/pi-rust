//! Walks of the ownership tree: owner tasks and conversations, cascades, idle scopes, and owned live work. The walks
//! down use the indexes of [`super::index`]; the walks up use the ownership fields of known tasks.

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

use super::{SchedState, Scheduler, cancellation_intent};
use crate::errors::DurableResult;
use crate::harness::util::{lock, scan_all};
use crate::ids::{ConversationId, TaskId};
use crate::session::Tx;
use crate::types::{SubmissionQuery, SubmissionStatus, SubmissionType, TaskRecord};

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

/// A node of the ownership tree: a task, or a conversation. A walk up continues at the parent of a task, which is
/// its owner task or its conversation, and at the owner task of a conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Node {
    Task(TaskId),
    Conversation(ConversationId),
}

/// Where a walk up the ownership tree continues.
pub(crate) type Up = Node;

impl Node {
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
    /// Links to the staged tasks the index does not list, by parent node, for walks down; built at first use.
    pub(super) links: OnceLock<HashMap<Node, BTreeSet<Node>>>,
}

impl Overlay {
    pub fn of(tx: &Tx) -> Self {
        let tasks = tx.staged_tasks().into_iter().map(|record| (record.id, record)).collect();
        let edges = tx.staged_conversations().into_iter().map(|c| (c.id, c.owner.map(|o| o.task_id))).collect();
        Self { tasks, edges, links: OnceLock::new() }
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
        // The conversation now hangs from its owner, or is a root; with nothing below it, it waits for the sweep.
        self.relist(Node::Conversation(conversation_id));
        self.drop_if_unneeded(Node::Conversation(conversation_id));
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
        // Nothing cancels, the common case: no walk.
        if self.intent.is_empty() {
            return None;
        }
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
}

impl Scheduler {
    /// Sweep in a job of its own on the Session line, after the operation underway: a rejected commit, one with
    /// nothing to write, and a read publish nothing, so the sweep at the end of `observe` would not run for what they
    /// loaded.
    pub(crate) fn schedule_sweep(&self) {
        {
            let mut state = lock(&self.state);
            if state.sweep_scheduled || state.closing || state.drop_queue.is_empty() {
                return;
            }
            state.sweep_scheduled = true;
        }
        let Ok(job) = self.arc() else { return };
        let sweep = self.core.session.read_on_line(async move {
            let mut state = lock(&job.state);
            state.sweep_scheduled = false;
            state.sweep();
            Ok(())
        });
        // Fails only once the Session is closing or failed, when nothing is kept anyway.
        tokio::spawn(async move {
            let _ = sweep.await;
        });
    }

    /// Load the owner chains of the tasks in `unloaded` and, with `queued`, of every conversation with queued
    /// inputs; returns the latter. Reads committed Storage directly, so it may run inside a commit callback.
    pub(crate) async fn load_scopes(&self, queued: bool) -> DurableResult<Vec<ConversationId>> {
        let pending: Vec<TaskId> = lock(&self.state).unloaded.iter().copied().collect();
        for id in pending {
            // An earlier load in this loop may have passed it.
            let unloaded = lock(&self.state).unloaded.contains(&id);
            if unloaded {
                self.load_chain(Up::Task(id), None).await?;
            }
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
        for submission in submissions.into_iter().filter(|submission| submission.kind == SubmissionType::Input) {
            if !conversations.contains(&submission.conversation_id) {
                conversations.push(submission.conversation_id);
            }
        }
        for id in &conversations {
            self.load_chain(Up::Conversation(*id), None).await?;
        }
        Ok(conversations)
    }

    /// Load the owner edges and task nodes from `start` up to its ownerless root. A known task outside `unloaded` ends
    /// the walk, as its chain is loaded; the `unloaded` tasks passed have a loaded chain afterwards. Loading a node can
    /// reveal a cancelled owner, so it makes the next reconcile derive cascade marks.
    pub(crate) async fn load_chain(&self, start: Up, overlay: Option<&Overlay>) -> DurableResult<()> {
        let loaded = self.load_chain_nodes(start, overlay).await;
        self.schedule_sweep();
        loaded
    }

    async fn load_chain_nodes(&self, start: Up, overlay: Option<&Overlay>) -> DurableResult<()> {
        let storage = self.core.session.storage();
        let mut passed = Vec::new();
        let mut at = Some(start);
        while let Some(current) = at {
            match current {
                Up::Task(id) => {
                    let known = {
                        let state = lock(&self.state);
                        let node = state.live.get(&id).map(TaskNode::of).or_else(|| state.settled.get(&id).copied());
                        // Created in this commit: the load from its own record covers what is above it.
                        if node.is_none() && overlay.is_some_and(|overlay| overlay.tasks.contains_key(&id)) {
                            break;
                        }
                        if node.is_some() && !state.unloaded.contains(&id) {
                            break;
                        }
                        node
                    };
                    let node = match known {
                        Some(node) => node,
                        None => {
                            let Some(record) = storage.task(id, &self.context).await? else { return Ok(()) };
                            let node = TaskNode::of(&record);
                            if record.is_terminal() {
                                lock(&self.state).settle(id, node);
                            }
                            self.schedule_reconcile();
                            node
                        }
                    };
                    passed.push(id);
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
                            self.schedule_reconcile();
                            edge
                        }
                    };
                    at = edge.map(Up::Task);
                }
            }
        }
        let mut state = lock(&self.state);
        for id in passed {
            state.unloaded.remove(&id);
        }
        Ok(())
    }
}
