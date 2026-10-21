//! The scheduler's in-memory indexes (upstream `scheduler.ts`: `#below`, `#roots`, `#unloaded`, `#intent`, `#runnable`,
//! `#finalizeChecks`, `#waiters`). No pass visits every live task: what lies below a node is a walk down from it, and
//! the sets hold each pass's candidates. No walk recurses, as ownership chains can be deeper than the call stack.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::ControlFlow;

use super::ownership::{Node, Overlay, Scope, TaskNode, Up};
use super::run::not_due;
use super::{SchedState, cancellation_intent};
use crate::ids::{ConversationId, TaskId};
use crate::types::{TaskRecord, TaskState, TaskStatus};

pub(super) fn add_to<K: Ord, V: Ord>(map: &mut BTreeMap<K, BTreeSet<V>>, key: K, value: V) {
    map.entry(key).or_default().insert(value);
}

pub(super) fn delete_from<K: Ord, V: Ord>(map: &mut BTreeMap<K, BTreeSet<V>>, key: &K, value: &V) {
    let Some(values) = map.get_mut(key) else { return };
    values.remove(value);
    if values.is_empty() {
        map.remove(key);
    }
}

/// The live task nearest above ended work, as far as a decision about it needs.
pub(super) struct Above {
    pub id: TaskId,
    pub completing: bool,
    pub abort_requested: bool,
}

/// How a walk down proceeds.
#[derive(Default)]
pub(super) struct Walk<'a> {
    /// Staged tasks replace committed records, and the commit's new tasks are reached too.
    pub overlay: Option<&'a Overlay>,
    /// Enter background tasks as well; they are reached either way.
    pub cross_background: bool,
    /// Tasks it rejects are reached but not entered.
    pub enter: Option<&'a dyn Fn(TaskId) -> bool>,
}

/// A task a walk down reached: its ownership fields, and its record when it is live.
pub(super) struct Reached<'a> {
    pub id: TaskId,
    pub node: TaskNode,
    pub live: Option<&'a TaskRecord>,
}

impl SchedState {
    /// Copies of the live tasks `owner` owns directly, in id order; each lies listed below its owner.
    pub fn owned_directly(&self, owner: TaskId) -> Vec<TaskRecord> {
        let nodes = self.below.get(&Node::Task(owner)).into_iter().flatten();
        let live = nodes.filter_map(|node| match node {
            Node::Task(id) => self.live.get(id),
            Node::Conversation(_) => None,
        });
        live.filter(|record| record.owner == Some(owner)).cloned().collect()
    }

    /// Whether the chain above `node` is loaded, without walking: known tasks outside `unloaded` have a loaded chain.
    pub fn known(&self, node: Node) -> bool {
        match node {
            Node::Task(id) => {
                (self.live.contains_key(&id) || self.settled.contains_key(&id)) && !self.unloaded.contains(&id)
            }
            Node::Conversation(id) => match self.edges.get(&id) {
                Some(None) => true,
                Some(Some(owner)) => self.known(Node::Task(*owner)),
                None => false,
            },
        }
    }

    /// Bring the indexes in line with a task's committed record. A task that ended may free the nearest live task above
    /// it: one holding its outcome finalizes, an abort-marked one runs its abort handler; its waiters may run.
    pub fn track(&mut self, previous: Option<&TaskRecord>, record: &TaskRecord) {
        let id = record.id;
        if let Some(TaskState::Waiting { on, .. }) = previous.map(|previous| &previous.state) {
            for task in on {
                delete_from(&mut self.waiters, task, &id);
            }
        }
        if record.is_terminal() {
            let node = TaskNode::of(record);
            self.live.remove(&id);
            self.intent.remove(&id);
            self.runnable.remove(&id);
            self.finalize_checks.remove(&id);
            self.sleeping.remove(&id);
            self.parked.remove(&id);
            // A task that ended unseen counts as known once, so that forgetting it nets out.
            if previous.is_none() && !self.settled.contains_key(&id) {
                self.count_child(&node, 1);
            }
            self.settled.insert(id, node);
            // Before anything is forgotten: the walk up to the task it held passes the ended tasks between them. Its chain
            // is loaded, as every commit that ends a task loads the chains of `unloaded` first.
            self.release(&node, None);
            let waiters: Vec<TaskId> = self.waiters.get(&id).into_iter().flatten().copied().collect();
            self.runnable.extend(waiters);
            self.relist(Node::Task(id));
            self.forget(id);
            return;
        }
        self.live.insert(id, record.clone());
        if previous.is_none() {
            if !self.known(Node::of(record)) {
                self.unloaded.insert(id);
            }
            self.count_child(&TaskNode::of(record), 1);
            self.relist(Node::Task(id));
        }
        if cancellation_intent(record) {
            self.intent.insert(id);
        } else {
            self.intent.remove(&id);
        }
        if record.status() == TaskStatus::Completing {
            self.runnable.remove(&id);
            self.finalize_checks.insert(id);
        } else {
            self.runnable.insert(id);
        }
        if let TaskState::Waiting { on, .. } = &record.state {
            for task in on {
                add_to(&mut self.waiters, *task, id);
            }
        }
        if record.status() == TaskStatus::Sleeping {
            self.sleeping.insert(id);
        } else {
            self.sleeping.remove(&id);
        }
    }

    /// Make every sleeping task whose time has come a reservation candidate.
    pub fn wake_due(&mut self, now: f64) {
        for id in &self.sleeping {
            if self.live.get(id).is_some_and(|record| !not_due(record, now)) {
                self.runnable.insert(*id);
            }
        }
    }

    /// Make parked tasks that a raised restart limit no longer parks candidates again; whether any were.
    pub fn unpark_raised(&mut self, limit: f64) -> bool {
        let mut unparked = false;
        for id in self.parked.clone() {
            let record = self.live.get(&id);
            if record.is_some_and(|record| is_parked(record, limit)) {
                continue;
            }
            self.parked.remove(&id);
            if record.is_some() {
                self.runnable.insert(id);
                unparked = true;
            }
        }
        unparked
    }

    /// The nearest live task above ended ordinary work may be free now: one holding its outcome, or an aborting one.
    pub fn release(&mut self, node: &TaskNode, overlay: Option<&Overlay>) {
        match self.nearest_live_above(node, overlay) {
            Some(above) if above.completing => {
                self.finalize_checks.insert(above.id);
            }
            Some(above) if above.abort_requested => {
                self.runnable.insert(above.id);
            }
            _ => {}
        }
    }

    /// The nearest live task above ended ordinary work `node`, in the overlay's view: the one that work held, if any.
    /// Work at or below a background task holds nothing above it.
    pub(super) fn nearest_live_above(&self, node: &TaskNode, overlay: Option<&Overlay>) -> Option<Above> {
        let mut current = *node;
        while !current.background {
            let owner = match current.owner {
                Some(owner) => owner,
                None => self.edge(current.conversation_id, overlay).flatten()?,
            };
            let record = overlay.and_then(|overlay| overlay.tasks.get(&owner)).or_else(|| self.live.get(&owner));
            if let Some(record) = record.filter(|record| !record.is_terminal()) {
                let (completing, abort_requested) = (record.status() == TaskStatus::Completing, record.abort_requested);
                return Some(Above { id: owner, completing, abort_requested });
            }
            current = record.map(TaskNode::of).or_else(|| self.settled.get(&owner).copied())?;
        }
        None
    }

    /// Keep a terminal task a chain load read, so walks pass through it; its own chain is not loaded yet.
    pub fn settle(&mut self, id: TaskId, node: TaskNode) {
        self.settled.insert(id, node);
        self.pinned.insert(id);
        self.unloaded.insert(id);
        self.count_child(&node, 1);
        self.relist(Node::Task(id));
    }

    /// Count a task that became known in its owner task's `known_children`, or one that is forgotten out of it.
    fn count_child(&mut self, node: &TaskNode, delta: isize) {
        let Some(owner) = node.owner else { return };
        let count = self.known_children.get(&owner).copied().unwrap_or(0).saturating_add_signed(delta);
        if count == 0 {
            self.known_children.remove(&owner);
        } else {
            self.known_children.insert(owner, count);
        }
    }

    /// Forget ended task `id` unless walks up still need it: it owns a loaded conversation, a chain load read it, or it
    /// owns a known task; then its owner, as far up as that changes. A loop: chains can be deeper than the call stack.
    fn forget(&mut self, id: TaskId) {
        let mut at = Some(id);
        while let Some(current) = at {
            let Some(node) = self.settled.get(&current).copied() else { return };
            if self.live.contains_key(&current)
                || self.conversation_owners.contains(&current)
                || self.pinned.contains(&current)
                || self.known_children.contains_key(&current)
                || self.below.contains_key(&Node::Task(current))
            {
                return;
            }
            self.settled.remove(&current);
            self.unloaded.remove(&current);
            self.count_child(&node, -1);
            at = node.owner;
        }
    }

    /// List `start` under its parent node in `below` while it is a live task or has nodes listed below it, and unlist it
    /// otherwise; then its parent, as far up as that changes. So `below` holds exactly the tree around live work. An
    /// ended parent that loses its last listed node may be forgotten, once the walk up is done.
    pub fn relist(&mut self, start: Node) {
        let mut emptied = Vec::new();
        let mut node = start;
        loop {
            let needed =
                matches!(node, Node::Task(id) if self.live.contains_key(&id)) || self.below.contains_key(&node);
            let parent = match node {
                Node::Conversation(id) => match self.edges.get(&id) {
                    None => break,
                    Some(None) => None,
                    Some(Some(owner)) => Some(Node::Task(*owner)),
                },
                Node::Task(id) => {
                    let known = self.live.get(&id).map(TaskNode::of).or_else(|| self.settled.get(&id).copied());
                    let Some(known) = known else { break };
                    Some(Node::of_node(&known))
                }
            };
            let Some(parent) = parent else {
                if let Node::Conversation(root) = node {
                    if needed {
                        self.roots.insert(root);
                    } else {
                        self.roots.remove(&root);
                    }
                }
                break;
            };
            let listed = self.below.get(&parent).is_some_and(|children| children.contains(&node));
            if listed == needed {
                break;
            }
            if needed {
                let first = !self.below.contains_key(&parent);
                add_to(&mut self.below, parent, node);
                if !first {
                    break;
                }
            } else {
                delete_from(&mut self.below, &parent, &node);
                if self.below.contains_key(&parent) {
                    break;
                }
                if let Node::Task(id) = parent {
                    emptied.push(id);
                }
            }
            node = parent;
        }
        for id in emptied {
            self.forget(id);
        }
    }

    /// The tasks below `start` that a walk down reaches: those `below` lists and, under an overlay, the commit's new
    /// tasks, with its candidates in place of committed records. Background tasks are reached but not entered, unless
    /// `cross_background`, and neither are the tasks `enter` rejects. `visit` stops the walk by breaking.
    pub(super) fn walk_down<'a>(
        &'a self,
        start: Node,
        walk: &Walk<'a>,
        mut visit: impl FnMut(Reached<'a>) -> ControlFlow<()>,
    ) {
        let links = walk.overlay.map(|overlay| self.links(overlay));
        let mut pending = vec![start];
        while let Some(at) = pending.pop() {
            let listed = self.below.get(&at);
            let linked = links.and_then(|links| links.get(&at));
            for child in listed.into_iter().chain(linked).flatten() {
                let Node::Task(id) = *child else {
                    pending.push(*child);
                    continue;
                };
                let record = walk.overlay.and_then(|overlay| overlay.tasks.get(&id)).or_else(|| self.live.get(&id));
                let Some(node) = record.map(TaskNode::of).or_else(|| self.settled.get(&id).copied()) else { continue };
                let live = record.filter(|record| !record.is_terminal());
                if visit(Reached { id, node, live }).is_break() {
                    return;
                }
                if (walk.cross_background || !node.background) && walk.enter.is_none_or(|enter| enter(id)) {
                    pending.push(*child);
                }
            }
        }
    }

    /// The links walks down need for the commit's new tasks: each one under its parent node, then each node above it
    /// that `below` does not list yet, until one it lists, a live task (always listed), or the chain's end.
    fn links<'a>(&self, overlay: &'a Overlay) -> &'a HashMap<Node, BTreeSet<Node>> {
        overlay.links.get_or_init(|| {
            let mut links: HashMap<Node, BTreeSet<Node>> = HashMap::new();
            for record in overlay.tasks.values() {
                if self.live.contains_key(&record.id) || self.settled.contains_key(&record.id) {
                    continue;
                }
                let mut child = Node::Task(record.id);
                let mut parent = Some(Node::of(record));
                while let Some(current) = parent {
                    if links.get(&current).is_some_and(|children| children.contains(&child)) {
                        break;
                    }
                    links.entry(current).or_default().insert(child);
                    if let Node::Task(id) = current
                        && (self.live.contains_key(&id) || overlay.tasks.contains_key(&id))
                    {
                        break;
                    }
                    let above = match current {
                        Node::Conversation(id) => self.edge(id, Some(overlay)).flatten().map(Node::Task),
                        Node::Task(id) => self.settled.get(&id).map(Node::of_node),
                    };
                    if above.is_some_and(|above| self.below.get(&above).is_some_and(|set| set.contains(&current))) {
                        break;
                    }
                    child = current;
                    parent = above;
                }
            }
            links
        })
    }

    /// The live ordinary owned work of `owner`: the live non-background tasks below it that a walk down reaches without
    /// entering a background task; with `first`, only the first. Candidates in `overlay` replace committed records; the
    /// walk does not enter tasks in `done`, which have none. Owner chains must be loaded.
    pub fn owned_live(
        &self,
        owner: TaskId,
        overlay: Option<&Overlay>,
        first: bool,
        done: Option<&HashSet<TaskId>>,
    ) -> Vec<TaskId> {
        let enter = |id: TaskId| done.is_none_or(|done| !done.contains(&id));
        let walk = Walk { overlay, cross_background: false, enter: Some(&enter) };
        let mut found = Vec::new();
        self.walk_down(Node::Task(owner), &walk, |reached| {
            if reached.live.is_none() || reached.node.background {
                return ControlFlow::Continue(());
            }
            found.push(reached.id);
            if first { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
        });
        found
    }

    pub fn has_owned_live(&self, owner: TaskId, overlay: Option<&Overlay>, done: Option<&HashSet<TaskId>>) -> bool {
        !self.owned_live(owner, overlay, true, done).is_empty()
    }

    /// No live non-background task in the scope; a task whose owner edges are not loaded yet counts as inside.
    pub fn idle(&self, conversation_id: Option<ConversationId>) -> bool {
        let scope = conversation_id.map_or(Scope::Roots, Scope::Conversation);
        // Tasks whose chain is not loaded are not listed below their scope yet; one counts as inside unless its walk up
        // decides otherwise.
        for id in &self.unloaded {
            let Some(record) = self.live.get(id) else { continue };
            if !record.background && self.in_scope(Up::of(record), scope, false) != Some(false) {
                return false;
            }
        }
        let starts: Vec<ConversationId> = match conversation_id {
            Some(id) => vec![id],
            None => self.roots.iter().copied().collect(),
        };
        let mut busy = false;
        for start in starts {
            self.walk_down(Node::Conversation(start), &Walk::default(), |reached| {
                busy = reached.live.is_some() && !reached.node.background;
                if busy { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
            });
            if busy {
                return false;
            }
        }
        true
    }
}

/// A task cut off `limit` times in a row; an abort mark still runs its abort handler.
pub(super) fn is_parked(record: &TaskRecord, limit: f64) -> bool {
    !record.abort_requested && record.restarts.is_some_and(|restarts| f64::from(restarts) >= limit)
}
