//! The task graph (spec section 9.4): every live task of the Session with its durable status, kept in step with
//! commit publications (upstream `harness/task-graph.ts`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use pi_chord::delta::{Op, Path, Seg, apply_immutable};
use pi_chord::state::{ReplicatedStateSourceOptions, attach_replicated_state_source};
use pi_chord::{BACKGROUND_CONTEXT, Context, Json, JsonMap, Watch, WatchChange, without_abort_signal};

use super::scheduler::{BlockedSubject, Scheduler};
use super::types::BlockedReason;
use super::util::{cancellation, closed_error, lock, scan_all};
use crate::errors::{DurableError, DurableResult};
use crate::ids::ConversationId;
use crate::session::{
    CommittedObserver, CommittedStateSource, DocumentState, GuardedObserver, Observation, ObservedCommit, Observer,
    Release, Session,
};
use crate::types::{CommitChange, ConversationQuery, TaskQuery, TaskRecord, TaskState, TaskStatus};

/// `{ tasks }`: every live task keyed by its decimal ID. A node is `{ id, kind, conversationId, owner?, background,
/// abortRequested, state, blocked?, conversations }`; `state` is `{ status, phase }`, `{ status: "waiting", phase, on,
/// policy }`, `{ status: "sleeping", phase, until }`, or `{ status: "completing", outcome }` with the outcome's status.
/// `blocked` means no registered definition takes it (`missing_task`, `task_too_old`, `migration_failed`); it follows registry changes.
pub type TaskGraph = Json;

/// A serialized exact-frame watch of the task graph.
pub type TaskGraphWatch = Watch<DurableError>;

const LIVE_STATUSES: [TaskStatus; 5] =
    [TaskStatus::Pending, TaskStatus::Running, TaskStatus::Waiting, TaskStatus::Sleeping, TaskStatus::Completing];
const SCAN_PAGE_SIZE: usize = 256;

struct Mount {
    value: Json,
    /// Watches, states, and guarded host observers (`observe_task_graph()`).
    observers: Vec<(u64, Arc<dyn CommittedObserver>)>,
    /// What the blocked reason reads of each live task, so a registry change can derive it again without payloads.
    records: HashMap<String, BlockedSubject>,
}

/// The Harness's task graph mount: built on the Session line by its first observer and dropped with its last. It
/// advances from the Session's commit publications, which are durable.
pub(crate) struct TaskGraphView {
    session: Session,
    scheduler: Weak<Scheduler>,
    mount: Mutex<Option<Mount>>,
    closed: AtomicBool,
    next: AtomicU64,
}

impl TaskGraphView {
    pub fn new(session: Session, scheduler: &Arc<Scheduler>) -> DurableResult<Arc<Self>> {
        let view = Arc::new(Self {
            session: session.clone(),
            scheduler: Arc::downgrade(scheduler),
            mount: Mutex::default(),
            closed: AtomicBool::new(false),
            next: AtomicU64::new(0),
        });
        let weak = Arc::downgrade(&view);
        session.observe_commits(move |publication, context| {
            if let Some(view) = weak.upgrade() {
                view.advance(publication, context);
            }
            Ok(())
        })?;
        let weak = Arc::downgrade(&view);
        scheduler.subscribe_blocked(Arc::new(move || {
            if let Some(view) = weak.upgrade() {
                view.refresh_blocked();
            }
        }));
        let weak = Arc::downgrade(&view);
        session.subscribe_close(move || {
            let Some(view) = weak.upgrade() else { return };
            view.closed.store(true, Ordering::SeqCst);
            let mount = lock(&view.mount).take();
            let failure = view.session.failure();
            for (_, observer) in mount.into_iter().flat_map(|mount| mount.observers) {
                observer.end(failure.clone());
            }
        })?;
        Ok(view)
    }

    /// A disposable read-only Chord state of the graph.
    pub async fn state(self: &Arc<Self>, context: &Context) -> DurableResult<DocumentState> {
        let (source, release) = self
            .attach(context, |value, release| {
                let source = CommittedStateSource::new(value.clone(), release);
                (source.clone() as Arc<dyn CommittedObserver>, source)
            })
            .await?;
        let session = self.session.clone();
        let on_error = Arc::new(move |error: pi_chord::ChordError| session.report(&error.into()));
        match attach_replicated_state_source(source.as_ref(), ReplicatedStateSourceOptions { on_error: Some(on_error) })
        {
            Ok(state) => Ok(state),
            Err(error) => {
                release.run();
                Err(error.into())
            }
        }
    }

    /// Attach a synchronous observer of the graph (`Harness::observe_task_graph()`), behind its guard; `context`
    /// governs only the attachment.
    pub async fn observe(
        self: &Arc<Self>,
        observer: Arc<dyn Observer>,
        context: &Context,
    ) -> DurableResult<Observation> {
        let session = self.session.clone();
        let ((value, guarded), _) = self
            .attach(context, move |value, release| {
                let guarded = GuardedObserver::new(observer, move || release.run(), move |error| session.report(error));
                (guarded.clone() as Arc<dyn CommittedObserver>, (value.clone(), guarded))
            })
            .await?;
        if context.is_cancelled() {
            guarded.detach();
            return Err(cancellation(context));
        }
        Ok(Observation::new(value, Arc::new(move || guarded.detach())))
    }

    /// A serialized exact-frame watch of the graph, an observer of its mount; cancelling `context` stops it.
    pub async fn watch(self: &Arc<Self>, context: &Context) -> DurableResult<TaskGraphWatch> {
        let session = self.session.clone();
        let (watch, _) = self
            .attach(context, move |value, release| {
                let watch = Watch::new(value.clone(), move || release.run(), move |error| session.report(error));
                (Arc::new(watch.clone()) as Arc<dyn CommittedObserver>, watch)
            })
            .await?;
        if context.is_cancelled() {
            watch.cancel();
            return Err(cancellation(context));
        }
        if let Some(signal) = context.abort_signal() {
            watch.observe_cancellation(&signal)?;
        }
        Ok(watch)
    }

    /// Register an observer created from the current revision, atomically on the Session line.
    async fn attach<T, F>(self: &Arc<Self>, context: &Context, create: F) -> DurableResult<(T, Arc<Release>)>
    where
        T: Send + 'static,
        F: FnOnce(&Json, Arc<Release>) -> (Arc<dyn CommittedObserver>, T) + Send + 'static,
    {
        let view = self.clone();
        let context = context.clone();
        self.session
            .read_on_line(async move {
                let mounted = lock(&view.mount).as_ref().map(|mount| (mount.value.clone(), mount.records.clone()));
                let (value, records) = match mounted {
                    Some(mounted) => mounted,
                    None => view.build(&context).await?,
                };
                let key = view.next.fetch_add(1, Ordering::SeqCst);
                let release = Release::new();
                let weak = Arc::downgrade(&view);
                release.push(move || {
                    let Some(view) = weak.upgrade() else { return };
                    let mut mount = lock(&view.mount);
                    if let Some(current) = mount.as_mut() {
                        current.observers.retain(|(other, _)| *other != key);
                        if current.observers.is_empty() {
                            *mount = None;
                        }
                    }
                });
                let (observer, result) = create(&value, release.clone());
                // Close or cancellation may begin while the mount builds; register nothing then.
                if view.closed.load(Ordering::SeqCst) {
                    return Err(closed_error(&view.session));
                }
                if context.is_cancelled() {
                    return Err(cancellation(&context));
                }
                let mut mount = lock(&view.mount);
                if mount.is_none() {
                    *mount = Some(Mount { value, observers: Vec::new(), records });
                }
                if let Some(mount) = mount.as_mut() {
                    mount.observers.push((key, observer));
                }
                Ok((result, release))
            })
            .await
    }

    fn blocked(&self, record: &BlockedSubject) -> Option<BlockedReason> {
        self.scheduler.upgrade().and_then(|scheduler| scheduler.blocked_reason(record))
    }

    /// On the Session line: derive every node's blocked reason again, and publish the ones that changed.
    fn refresh_blocked(self: &Arc<Self>) {
        if lock(&self.mount).is_none() || self.closed.load(Ordering::SeqCst) {
            return;
        }
        let view = self.clone();
        let refresh = self.session.read_on_line(async move {
            view.publish_blocked();
            Ok(())
        });
        // Fails only once the Session is closing or failed.
        tokio::spawn(async move { drop(refresh.await) });
    }

    fn publish_blocked(&self) {
        let delivery = {
            let mut mount = lock(&self.mount);
            let Some(mount) = mount.as_mut() else { return };
            let mut ops = Vec::new();
            for (key, record) in &mount.records {
                let blocked = self.blocked(record);
                let shown =
                    mount.value.get("tasks").and_then(|tasks| tasks.get(key)).and_then(|node| node.get("blocked"));
                if shown.and_then(Json::as_str) == blocked.map(BlockedReason::as_str) {
                    continue;
                }
                ops.push(match blocked {
                    Some(reason) => Op::Set(path(&["tasks", key, "blocked"]), Json::from(reason.as_str())),
                    None => Op::Delete(path(&["tasks", key, "blocked"])),
                });
            }
            if ops.is_empty() {
                return;
            }
            match apply_immutable(Some(&mount.value), &ops) {
                Ok(value) => mount.value = value,
                Err(error) => return self.session.report(&error.into()),
            }
            let observers: Vec<_> = mount.observers.iter().map(|(_, observer)| observer.clone()).collect();
            (mount.value.clone(), ops, observers)
        };
        let (value, ops, observers) = delivery;
        let change = WatchChange::new(value, ops);
        for observer in observers {
            observer.advance(change.clone(), BACKGROUND_CONTEXT.clone());
        }
    }

    async fn build(&self, context: &Context) -> DurableResult<(Json, HashMap<String, BlockedSubject>)> {
        let storage = self.session.storage();
        let mut records = Vec::new();
        for status in LIVE_STATUSES {
            let query = TaskQuery { status: Some(status), ..TaskQuery::default() };
            let storage = &storage;
            let query = &query;
            records.extend(
                scan_all(
                    |cursor| async move { storage.scan_tasks(query, SCAN_PAGE_SIZE, cursor.as_ref(), context).await },
                )
                .await?,
            );
        }
        records.sort_by_key(|record| record.id);
        let mut tasks = JsonMap::new();
        let mut subjects = HashMap::new();
        for record in records {
            let query = ConversationQuery { owner_task_id: Some(record.id), ..ConversationQuery::default() };
            let (storage, query) = (&storage, &query);
            let owned = scan_all(|cursor| async move {
                storage.scan_conversations(query, SCAN_PAGE_SIZE, cursor.as_ref(), context).await
            })
            .await?;
            let mut conversations: Vec<ConversationId> = owned.iter().map(|conversation| conversation.id).collect();
            conversations.sort();
            let subject = BlockedSubject::of(&record);
            tasks.insert(record.id.to_string().as_str(), node_of(&record, &conversations, self.blocked(&subject))?);
            subjects.insert(record.id.to_string(), subject);
        }
        let mut graph = JsonMap::new();
        graph.insert("tasks", Json::object(tasks));
        Ok((Json::object(graph), subjects))
    }

    /// Derive the mount's operations from one publication, apply them, and hand the revision to every observer.
    fn advance(&self, publication: &ObservedCommit, context: &Context) {
        // A commit held in memory changes documents only, which the graph does not show.
        if publication.is_memory() {
            return;
        }
        let delivery = {
            let mut mount = lock(&self.mount);
            let Some(mount) = mount.as_mut() else { return };
            match graph_ops(&mount.value, &mut mount.records, publication, &|record| self.blocked(record)) {
                Ok(ops) if ops.is_empty() => return,
                Ok(ops) => match apply_immutable(Some(&mount.value), &ops) {
                    Ok(value) => {
                        mount.value = value;
                        let observers: Vec<_> = mount.observers.iter().map(|(_, observer)| observer.clone()).collect();
                        (mount.value.clone(), ops, observers)
                    }
                    Err(error) => return self.session.report(&error.into()),
                },
                Err(error) => return self.session.report(&error),
            }
        };
        let (value, ops, observers) = delivery;
        let (change, frame_context) = (WatchChange::new(value, ops), without_abort_signal(context));
        for observer in observers {
            observer.advance(change.clone(), frame_context.clone());
        }
    }
}

fn path(keys: &[&str]) -> Path {
    keys.iter().map(|key| Seg::Key(Arc::from(*key))).collect()
}

/// The operations one publication makes on `graph`: task nodes set or deleted, then owned conversations added.
fn graph_ops(
    graph: &Json,
    records: &mut HashMap<String, BlockedSubject>,
    publication: &ObservedCommit,
    blocked: &dyn Fn(&BlockedSubject) -> Option<BlockedReason>,
) -> DurableResult<Vec<Op>> {
    let mut ops = Vec::new();
    // Nodes this publication set or deleted, over the mount's value.
    let mut changed: Vec<(String, Option<Json>)> = Vec::new();
    let node = |changed: &[(String, Option<Json>)], key: &str| -> Option<Json> {
        match changed.iter().rev().find(|(other, _)| other == key) {
            Some((_, value)) => value.clone(),
            None => graph.get("tasks").and_then(|tasks| tasks.get(key)).cloned(),
        }
    };
    for change in publication.changes() {
        let CommitChange::Task(record) = change else { continue };
        let key = record.id.to_string();
        let previous = node(&changed, &key);
        if matches!(record.state, TaskState::Terminal { .. }) {
            records.remove(&key);
            if previous.is_none() {
                continue;
            }
            ops.push(Op::Delete(path(&["tasks", &key])));
            changed.push((key, None));
            continue;
        }
        let conversations = previous.as_ref().map(owned_of).unwrap_or_default();
        let subject = BlockedSubject::of(record);
        let next = node_of(record, &conversations, blocked(&subject))?;
        records.insert(key.clone(), subject);
        if previous.as_ref() == Some(&next) {
            continue;
        }
        ops.push(Op::Set(path(&["tasks", &key]), next.clone()));
        changed.push((key, Some(next)));
    }
    // After the tasks, so a conversation created with its owner task in one commit finds the owner's node. Change
    // order within a publication is unspecified, so each owner's list is sorted again.
    let mut created: Vec<(String, Vec<ConversationId>)> = Vec::new();
    for change in publication.changes() {
        let CommitChange::Conversation(conversation) = change else { continue };
        let Some(owner) = conversation.owner else { continue };
        let key = owner.task_id.to_string();
        if node(&changed, &key).is_none() {
            continue;
        }
        match created.iter_mut().find(|(other, _)| *other == key) {
            Some((_, ids)) => ids.push(conversation.id),
            None => created.push((key, vec![conversation.id])),
        }
    }
    for (key, ids) in created {
        let mut conversations = node(&changed, &key).as_ref().map(owned_of).unwrap_or_default();
        conversations.extend(ids);
        conversations.sort();
        ops.push(Op::Set(path(&["tasks", &key, "conversations"]), crate::documents::encode(&conversations)?));
    }
    Ok(ops)
}

fn owned_of(node: &Json) -> Vec<ConversationId> {
    let ids = node.get("conversations").and_then(Json::as_array).cloned().unwrap_or_default();
    ids.iter().filter_map(Json::as_u64).map(ConversationId).collect()
}

fn node_of(
    record: &TaskRecord,
    conversations: &[ConversationId],
    blocked: Option<BlockedReason>,
) -> DurableResult<Json> {
    use crate::documents::encode;
    let mut node = JsonMap::new();
    node.insert("id", encode(&record.id)?);
    node.insert("kind", Json::from(record.kind.as_str()));
    node.insert("conversationId", encode(&record.conversation_id)?);
    if let Some(owner) = record.owner {
        node.insert("owner", encode(&owner)?);
    }
    node.insert("background", Json::from(record.background));
    node.insert("abortRequested", Json::from(record.abort_requested));
    node.insert("state", state_of(&record.state)?);
    if let Some(reason) = blocked {
        node.insert("blocked", Json::from(reason.as_str()));
    }
    node.insert("conversations", encode(&conversations)?);
    Ok(Json::object(node))
}

fn state_of(state: &TaskState) -> DurableResult<Json> {
    let mut value = JsonMap::new();
    match state {
        TaskState::Pending { checkpoint } | TaskState::Running { checkpoint } => {
            value.insert(
                "status",
                Json::from(if matches!(state, TaskState::Pending { .. }) { "pending" } else { "running" }),
            );
            value.insert("phase", phase_of(checkpoint));
        }
        TaskState::Waiting { checkpoint, on, policy } => {
            value.insert("status", Json::from("waiting"));
            value.insert("phase", phase_of(checkpoint));
            value.insert("on", crate::documents::encode(on)?);
            value.insert("policy", crate::documents::encode(policy)?);
        }
        TaskState::Sleeping { checkpoint, until } => {
            value.insert("status", Json::from("sleeping"));
            value.insert("phase", phase_of(checkpoint));
            value.insert("until", Json::from(*until));
        }
        // Terminal records never reach here: they leave the graph.
        TaskState::Completing { outcome } | TaskState::Terminal { outcome } => {
            let outcome = crate::documents::encode(outcome)?;
            value.insert("status", Json::from("completing"));
            value.insert("outcome", outcome.get("status").cloned().unwrap_or(Json::Null));
        }
    }
    Ok(Json::object(value))
}

fn phase_of(checkpoint: &Json) -> Json {
    checkpoint.get("phase").cloned().unwrap_or(Json::Null)
}
