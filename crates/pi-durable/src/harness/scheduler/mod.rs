//! Durable task scheduler of one Harness (upstream `harness/scheduler.ts`). `live` mirrors every committed live
//! task record, kept in step by the commit listener on the Session line. Every task change is decided and written
//! by one callback on the line; handlers and joins run off it.

mod observe;
mod ownership;
mod reconcile;
mod run;
mod runtime;
mod step;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use pi_chord::{AbortController, Context};
use tokio::sync::watch;

pub use runtime::{ConversationHandle, HookApi, TaskRuntime};

pub(crate) use ownership::{TaskNode, Up};

use super::context::ContextRange;
use super::core::HarnessCore;
use super::registry::RegistryUnsubscribe;
use super::submissions::Submissions;
use super::task::Task;
use super::types::{AbortTaskResult, Wake};
use super::util::{Waiters, closed_error, lock, scan_all, with_context};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, TaskId};
use crate::session::{CommittedWatch, TransactionScope};
use crate::timers::Disarm;
use crate::types::{AbortReason, JoinPolicy, TaskOutcome, TaskQuery, TaskRecord, TaskState, TaskStatus};

const SCAN_PAGE_SIZE: usize = 256;
const LIVE_STATUSES: [TaskStatus; 5] =
    [TaskStatus::Pending, TaskStatus::Running, TaskStatus::Waiting, TaskStatus::Sleeping, TaskStatus::Completing];

/// Whether an invocation runs phases or the abort handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Run,
    Abort,
}

/// One in-memory execution of a task in run or abort mode.
pub(crate) struct Invocation {
    pub task_id: TaskId,
    pub conversation_id: ConversationId,
    pub mode: Mode,
    pub controller: AbortController,
    /// Context passed to handlers; cancelled by `controller`.
    pub context: Context,
    /// Watches taken through the runtime; stopped at invocation end.
    pub watches: Mutex<Vec<CommittedWatch>>,
    /// Timers armed through the runtime, by key; disarmed at invocation end.
    pub timers: Mutex<HashMap<u64, Disarm>>,
    pub next_timer: AtomicU64,
    pub ended: AtomicBool,
    pub done: watch::Sender<bool>,
}

impl Invocation {
    pub fn is_ended(&self) -> bool {
        self.ended.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn ended_error(&self) -> DurableError {
        DurableError::error(format!("Task {} invocation has ended", self.task_id))
    }

    pub fn check(&self) -> DurableResult<()> {
        if self.is_ended() { Err(self.ended_error()) } else { Ok(()) }
    }

    pub async fn finished(&self) {
        let mut done = self.done.subscribe();
        let _ = done.wait_for(|done| *done).await;
    }
}

/// A context range kept per conversation, and since when its conversation is idle.
pub(crate) struct KeptContext {
    pub range: Arc<ContextRange>,
    pub idle_since: Option<f64>,
}

#[derive(Default)]
pub(crate) struct SchedState {
    pub live: BTreeMap<TaskId, TaskRecord>,
    pub contexts: HashMap<ConversationId, KeptContext>,
    /// Background timer for the earliest expiry of an idle context, unless the host declined it.
    pub expiry: Option<(f64, Disarm)>,
    /// Timer for the earliest sleep, without `on_wake` while scheduling runs.
    pub timer: Option<(f64, Disarm)>,
    /// Last value given to `on_wake`, so an unchanged one is not repeated.
    pub last_wake: Option<Wake>,
    pub wake_scheduled: bool,
    pub invocations: HashMap<TaskId, Arc<Invocation>>,
    /// Definition whose migration failed per task; retried only once the registry resolves another definition.
    pub failed_migrations: HashMap<TaskId, (Task, DurableError)>,
    /// Owner task of each loaded conversation, `None` when ownerless.
    pub edges: HashMap<ConversationId, Option<TaskId>>,
    pub conversation_owners: HashSet<TaskId>,
    /// Ownership fields of terminal tasks that walks pass through.
    pub settled: HashMap<TaskId, TaskNode>,
    pub fail_fast_checks: BTreeSet<TaskId>,
    /// Live `abandon_on_restart` tasks found at open; the first reservation pass abort-marks them.
    pub abandoned: BTreeSet<TaskId>,
    pub reconcile_scheduled: bool,
    pub cascade_pending: bool,
    pub unsubscribe_registry: Option<RegistryUnsubscribe>,
    pub enabled: bool,
    pub closing: bool,
    pub dirty: bool,
    pub draining: bool,
}

pub(crate) struct Scheduler {
    pub core: Arc<HarnessCore>,
    /// Context of scheduler commits and invocations; carries no caller cancellation.
    pub context: Context,
    pub state: Mutex<SchedState>,
    pub task_waiters: Waiters<TaskId, TaskRecord>,
    pub idle_waiters: Waiters<Option<ConversationId>, ()>,
    pub submissions: OnceLock<Weak<Submissions>>,
    pub weak: Weak<Scheduler>,
}

impl Scheduler {
    pub fn new(core: Arc<HarnessCore>, context: Context) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            core,
            context,
            state: Mutex::new(SchedState::default()),
            task_waiters: Waiters::default(),
            idle_waiters: Waiters::default(),
            submissions: OnceLock::new(),
            weak: weak.clone(),
        })
    }

    pub fn arc(&self) -> DurableResult<Arc<Self>> {
        self.weak.upgrade().ok_or_else(|| closed_error(&self.core.session))
    }

    pub fn closing(&self) -> bool {
        lock(&self.state).closing
    }

    /// Load live tasks and change surviving `running` tasks back to `pending`. Dispatches nothing.
    pub async fn open(&self, context: &Context) -> DurableResult<()> {
        let session = &self.core.session;
        let weak = self.weak.clone();
        session.observe_commits(move |publication, _| {
            if let Some(scheduler) = weak.upgrade() {
                scheduler.observe(publication);
            }
            Ok(())
        })?;
        let weak = self.weak.clone();
        session.subscribe_close(move || {
            if let Some(scheduler) = weak.upgrade() {
                scheduler.seal();
            }
        })?;
        let weak = self.weak.clone();
        let unsubscribe = self.core.registry.subscribe(Arc::new(move || {
            if let Some(scheduler) = weak.upgrade() {
                scheduler.kick();
            }
        }));
        lock(&self.state).unsubscribe_registry = Some(unsubscribe);
        let scheduler = self.arc()?;
        let opened = session.commit_with(
            move |tx| async move {
                let mut records = Vec::new();
                for status in LIVE_STATUSES {
                    let query = TaskQuery { status: Some(status), ..TaskQuery::default() };
                    let (tx, query) = (&tx, &query);
                    records.extend(scan_all(|cursor| tx.scan_tasks(query.clone(), SCAN_PAGE_SIZE, cursor)).await?);
                }
                let mut state = lock(&scheduler.state);
                for record in records {
                    if record.abandon_on_restart == Some(true) {
                        state.abandoned.insert(record.id);
                    }
                    match &record.state {
                        TaskState::Running { checkpoint } => {
                            let pending = TaskState::Pending { checkpoint: checkpoint.clone() };
                            tx.set_task(with_state(&record, pending))?;
                        }
                        TaskState::Waiting { policy: JoinPolicy::FailFast, .. } => {
                            state.fail_fast_checks.insert(record.id);
                        }
                        _ => {}
                    }
                    state.live.insert(record.id, record);
                }
                Ok(())
            },
            context,
            TransactionScope::default(),
        );
        opened.await?;
        // Derive abort marks a crash left unapplied below cancelled owners, and finalize held outcomes.
        lock(&self.state).cascade_pending = true;
        self.schedule_reconcile();
        self.schedule_wake();
        Ok(())
    }

    /// Enable scheduling, which also runs sleeping tasks that are due: a host wakeup, such as an alarm, calls this.
    /// Idempotent; the kick does nothing once closing.
    pub fn resume(&self) {
        lock(&self.state).enabled = true;
        self.kick();
    }

    /// When the Session next needs a process: `active` while some task can run now or is running, `at` for the
    /// earliest sleep not yet due. From memory; a task whose chain is not loaded yet counts as it would alone.
    pub fn wake(&self) -> Wake {
        let now = self.core.now();
        let (mut active, at, candidates) = {
            let state = lock(&self.state);
            let mut active = !state.invocations.is_empty();
            let mut at: Option<f64> = None;
            let mut owned = None;
            let mut candidates = Vec::new();
            for record in state.live.values() {
                match &record.state {
                    TaskState::Completing { .. } => continue,
                    TaskState::Waiting { on, .. }
                        if !record.abort_requested && on.iter().any(|id| state.live.contains_key(id)) =>
                    {
                        continue;
                    }
                    TaskState::Sleeping { until, .. } if !record.abort_requested && *until > now => {
                        at = Some(at.map_or(*until, |at| at.min(*until)));
                        continue;
                    }
                    _ => {}
                }
                if active {
                    continue;
                }
                if record.abort_requested {
                    // An abort invocation waits for the live work below it, which counts itself.
                    let owned = owned.get_or_insert_with(|| state.owned_live(None));
                    if owned.get(&record.id).is_some_and(|ids| !ids.is_empty()) {
                        continue;
                    }
                    if record.abort_reason.is_none() {
                        active = true;
                        continue;
                    }
                }
                candidates.push(record.clone());
            }
            (active, at, candidates)
        };
        // A task no definition can take waits for a registry change, except an abort-marked one, which is orphaned.
        if !active && !candidates.is_empty() {
            let snapshot = self.core.registry.snapshot();
            active = candidates.iter().any(|record| self.fit(record, snapshot.task(&record.kind)).is_ok());
        }
        Wake { active, at }
    }

    /// Signal every invocation and wait for them; writes nothing.
    pub async fn join(&self) {
        let invocations: Vec<_> = lock(&self.state).invocations.values().cloned().collect();
        for invocation in &invocations {
            invocation.controller.abort(None);
        }
        for invocation in invocations {
            invocation.finished().await;
        }
    }

    /// Commit the abort mark, or orphan a task no definition can take, then join the run invocation.
    pub async fn abort(&self, id: TaskId, context: &Context, keep_restart: bool) -> DurableResult<AbortTaskResult> {
        let scheduler = self.arc()?;
        let marked = self.core.session.commit_with(
            move |tx| async move {
                let Some(current) = tx.task(id).await? else {
                    return Err(DurableError::error(format!("Task {id} does not exist")));
                };
                if current.is_terminal() {
                    return Ok((AbortTaskResult::Terminal, None));
                }
                let invocation = lock(&scheduler.state).invocations.get(&id).cloned();
                if keep_restart && current.abort_reason == Some(AbortReason::Restart) {
                    return Ok((AbortTaskResult::Marked, None));
                }
                if invocation.is_none() && current.status() != TaskStatus::Completing {
                    scheduler.load_scopes(false).await?;
                    let owned = lock(&scheduler.state).owned_live(None).contains_key(&id);
                    if !owned {
                        let snapshot = scheduler.core.registry.snapshot();
                        if let Err(reason) = scheduler.resolve(&current, &snapshot) {
                            let outcome = TaskOutcome::Orphaned { reason: reason.as_str().into() };
                            scheduler.terminate(&tx, &current, outcome).await?;
                            return Ok((AbortTaskResult::Marked, None));
                        }
                    }
                }
                if !current.abort_requested || current.abort_reason.is_some() {
                    tx.set_task(with_abort_mark(&current, None))?;
                }
                let run = invocation.filter(|invocation| invocation.mode == Mode::Run);
                Ok((AbortTaskResult::Marked, run))
            },
            context,
            TransactionScope::default(),
        );
        let (result, run) = marked.await?;
        if let Some(run) = run {
            let failed = self.core.session.failed();
            let joined = async move {
                // A run that ignores its signal can outlive a failed Session, which ends the wait with its failure.
                tokio::select! {
                    () = run.finished() => Ok(()),
                    error = failed => Err(error),
                }
            };
            with_context(joined, context).await?;
        }
        Ok(result)
    }

    pub async fn wait_for_task(&self, id: TaskId, context: &Context) -> DurableResult<TaskRecord> {
        let scheduler = self.arc()?;
        let line_context = context.clone();
        let found = self.core.session.read_on_line(async move {
            let live = {
                let state = lock(&scheduler.state);
                if state.closing {
                    return Err(closed_error(&scheduler.core.session));
                }
                state.live.contains_key(&id)
            };
            if live {
                return Ok(Err(scheduler.task_waiters.add(id, &line_context)));
            }
            match scheduler.core.session.storage().task(id, &line_context).await? {
                Some(record) => Ok(Ok(record)),
                None => Err(DurableError::error(format!("Task {id} does not exist"))),
            }
        });
        match found.await? {
            Ok(record) => Ok(record),
            Err(wait) => wait.await,
        }
    }

    /// Resolve when ordinary traversal from the conversation, or from every ownerless one, reaches no live task.
    pub async fn wait_for_idle(&self, conversation_id: Option<ConversationId>, context: &Context) -> DurableResult<()> {
        let wait = {
            let state = lock(&self.state);
            if state.closing {
                return Err(closed_error(&self.core.session));
            }
            if state.idle(conversation_id) {
                return Ok(());
            }
            self.idle_waiters.add(conversation_id, context)
        };
        self.schedule_reconcile();
        wait.await
    }

    /// `Conversation.abort()`: withdraw queued inputs and mark every live non-background task of the scope.
    pub async fn abort_conversation(
        &self,
        conversation_id: ConversationId,
        background: bool,
        context: &Context,
    ) -> DurableResult<()> {
        let scheduler = self.arc()?;
        let reached = self.core.session.commit_with(
            move |tx| async move {
                let queued = scheduler.load_scopes(true).await?;
                let scope = ownership::Scope::Conversation(conversation_id);
                let (reached, withdraw) = {
                    let state = lock(&scheduler.state);
                    let mut reached = Vec::new();
                    for record in state.live.values() {
                        if record.background && !background {
                            continue;
                        }
                        if state.in_scope(Up::of(record), scope, background) != Some(true) {
                            continue;
                        }
                        reached.push(record.id);
                        if !record.abort_requested || record.abort_reason.is_some() {
                            tx.set_task(with_abort_mark(record, None))?;
                        }
                    }
                    let withdraw: Vec<_> = queued
                        .into_iter()
                        .filter(|id| state.in_scope(Up::Conversation(*id), scope, background) == Some(true))
                        .collect();
                    (reached, withdraw)
                };
                for id in withdraw {
                    super::inbox::withdraw_queued_inputs(&tx, id).await?;
                }
                Ok(reached)
            },
            context,
            TransactionScope::default(),
        );
        let reached = reached.await?;
        if background {
            for id in reached {
                self.wait_for_task(id, context).await?;
            }
        }
        self.wait_for_idle(Some(conversation_id), context).await
    }

    /// Fail the Session for a failure in one of the scheduler's own commits; ignored once closing.
    pub(crate) fn fail_session(&self, error: DurableError) {
        if !self.closing() {
            self.core.session.fail(error);
        }
    }
}

/// The record with an abort mark: `restart` for an abandonment; without a reason, a request.
pub(crate) fn with_abort_mark(record: &TaskRecord, reason: Option<AbortReason>) -> TaskRecord {
    TaskRecord { abort_requested: true, abort_reason: reason, ..record.clone() }
}

/// A live owner's durable cancellation intent: its abort mark, or a held outcome other than `completed`.
pub(crate) fn cancellation_intent(record: &TaskRecord) -> bool {
    !record.is_terminal() && (record.abort_requested || failed_outcome(record))
}

/// Whether the record holds or ended with an outcome other than `completed`.
pub(crate) fn failed_outcome(record: &TaskRecord) -> bool {
    record.state.outcome().is_some_and(|outcome| !matches!(outcome, TaskOutcome::Completed { .. }))
}

/// Replace a live record's state; memos disappear once an outcome is decided.
pub(crate) fn with_state(record: &TaskRecord, state: TaskState) -> TaskRecord {
    let decided = matches!(state, TaskState::Terminal { .. } | TaskState::Completing { .. });
    TaskRecord { memos: if decided { None } else { record.memos.clone() }, state, ..record.clone() }
}
