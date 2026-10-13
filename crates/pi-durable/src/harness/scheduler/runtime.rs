//! Operations of one task invocation (upstream `TaskRuntime`). Every operation fails after the invocation ends.

use std::any::Any;
use std::future::Future;
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use pi_chord::{AbortSignal, Context, Json, with_abort_signal};

use super::step::RunState;
use super::{Invocation, KeptContext, Scheduler};
use crate::documents::{DocFamilyToken, DocScope, DocToken, DocValue, RewindableDocScope};
use crate::entries::{Entry, TypedEntry};
use crate::env::SharedEnv;
use crate::errors::{DurableError, DurableResult};
use crate::harness::agent::agent_hooks;
use crate::harness::context::read_context_from;
use crate::harness::registry::RegistrySnapshot;
use crate::harness::settings::Settings;
use crate::harness::submissions::Submission;
use crate::harness::task::NextTaskState;
use crate::harness::types::{Agent, AbortTaskResult, ContextView, SubmissionDraft};
use crate::harness::util::{lock, with_context};
use crate::ids::{ConversationId, EntryId, TaskId};
use crate::models::Models;
use crate::session::{CommittedWatch, TransactionScope, Tx};
use crate::types::{EntryRecord, TaskOutcome, TaskRecord};

type AgentFuture = Shared<BoxFuture<'static, DurableResult<Agent>>>;

/// What a runtime reads for the phase handler it serves: the phase's snapshot and task, and its agent.
pub(crate) struct Phase {
    state: Arc<Mutex<RunState>>,
    agent: Mutex<Option<AgentFuture>>,
}

impl Phase {
    pub fn new(state: Arc<Mutex<RunState>>) -> Self {
        Self { state, agent: Mutex::new(None) }
    }

    /// Each phase handler resolves its agent afresh, at first use.
    pub fn reset_agent(&self) {
        *lock(&self.agent) = None;
    }
}

/// Operations of one task invocation; cheap to clone. Hooks receive it as their `HookApi`.
#[derive(Clone)]
pub struct TaskRuntime {
    pub(crate) scheduler: Arc<Scheduler>,
    pub(crate) invocation: Arc<Invocation>,
    phase: Arc<Phase>,
}

/// What a hook may use: committed reads and the asking task's memos.
pub type HookApi = TaskRuntime;

impl TaskRuntime {
    pub(crate) fn new(scheduler: Arc<Scheduler>, invocation: Arc<Invocation>, phase: Arc<Phase>) -> Self {
        Self { scheduler, invocation, phase }
    }

    pub fn task_id(&self) -> TaskId {
        self.invocation.task_id
    }

    pub fn conversation_id(&self) -> ConversationId {
        self.invocation.conversation_id
    }

    /// Aborted when the run is signalled by `abort_task()`, the Harness closes, or the invocation ends.
    pub fn signal(&self) -> AbortSignal {
        self.invocation.controller.signal()
    }

    /// Registry snapshot of the current phase; refreshed at every phase boundary.
    pub fn registry(&self) -> RegistrySnapshot {
        lock(&self.phase.state).snapshot.clone()
    }

    pub fn settings(&self) -> Settings {
        self.scheduler.core.settings()
    }

    pub fn models(&self) -> Arc<dyn Models> {
        self.scheduler.core.models.clone()
    }

    /// The task's conversation's agent, resolved at most once per phase, at first use.
    pub async fn agent(&self, context: &Context) -> DurableResult<Agent> {
        self.invocation.check()?;
        let shared = {
            let mut slot = lock(&self.phase.agent);
            match &*slot {
                Some(shared) => shared.clone(),
                None => {
                    let (scheduler, id) = (self.scheduler.clone(), self.invocation.conversation_id);
                    let (snapshot, context) = (self.registry(), self.invocation.context.clone());
                    let resolve = async move { scheduler.core.resolve_agent(id, Some(snapshot), &context).await };
                    let shared = resolve.boxed().shared();
                    *slot = Some(shared.clone());
                    shared
                }
            }
        };
        with_context(shared, context).await
    }

    /// Calls `HarnessOptions::env` for the task's conversation.
    pub async fn env(&self, context: &Context) -> DurableResult<Option<SharedEnv>> {
        self.invocation.check()?;
        self.scheduler.core.build_env(self.invocation.conversation_id, context).await
    }

    /// Handlers of type `H` for this task's name from the extensions its conversation selects, in order.
    pub async fn hooks<H: Any + Send + Sync>(&self, context: &Context) -> DurableResult<Vec<Arc<H>>> {
        let agent = self.agent(context).await?;
        let name = lock(&self.phase.state).task.name.clone();
        Ok(agent_hooks(&agent, &name).into_iter().filter_map(|handlers| handlers.downcast::<H>().ok()).collect())
    }

    /// A hook's failure: reported, unless the invocation was signalled, which propagates it.
    pub fn hook_failed(&self, error: DurableError) -> DurableResult<()> {
        if self.signal().aborted() {
            return Err(error);
        }
        self.scheduler.core.report(&error);
        Ok(())
    }

    /// Commit after rereading the task; a returned state replaces the task's state in the same commit.
    pub async fn commit<F, Fut>(&self, change: F, context: &Context) -> DurableResult<()>
    where
        F: FnOnce(Tx, TaskRecord) -> Fut + Send + 'static,
        Fut: Future<Output = DurableResult<Option<NextTaskState>>> + Send + 'static,
    {
        let (scheduler, invocation) = (self.scheduler.clone(), self.invocation.clone());
        self.gated(
            move |tx, current| async move {
                if let Some(next) = change(tx.clone(), current.clone()).await? {
                    scheduler.commit_state(&tx, &invocation, &current, next).await?;
                }
                Ok(())
            },
            context,
        )
        .await
    }

    /// Commit a change that returns a value and leaves the task's state as it is.
    pub async fn commit_map<T, F, Fut>(&self, change: F, context: &Context) -> DurableResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Tx) -> Fut + Send + 'static,
        Fut: Future<Output = DurableResult<T>> + Send + 'static,
    {
        self.gated(move |tx, _| change(tx), context).await
    }

    async fn gated<T, F, Fut>(&self, change: F, context: &Context) -> DurableResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Tx, TaskRecord) -> Fut + Send + 'static,
        Fut: Future<Output = DurableResult<T>> + Send + 'static,
    {
        self.invocation.check()?;
        let (scheduler, invocation) = (self.scheduler.clone(), self.invocation.clone());
        let scope = TransactionScope { conversation_id: Some(invocation.conversation_id), task_id: Some(invocation.task_id) };
        let committed = self.scheduler.core.session.commit_with(
            move |tx| async move {
                invocation.check()?;
                if scheduler.closing() {
                    return Err(crate::harness::util::closed_error(&scheduler.core.session));
                }
                let id = invocation.task_id;
                let found = lock(&scheduler.state).live.get(&id).cloned();
                let Some(current) = found else { return Err(DurableError::error(format!("Task {id} is terminal"))) };
                if current.status() != crate::types::TaskStatus::Running {
                    return Err(DurableError::error(format!("Task {id} is {}", current.status().as_str())));
                }
                if invocation.mode == super::Mode::Run && current.abort_requested {
                    return Err(DurableError::error(format!("Task {id} has a durable abort mark")));
                }
                change(tx, current).await
            },
            context,
            scope,
        );
        committed.await
    }

    /// Read a durable memo of this task.
    pub async fn memo(&self, name: &str) -> DurableResult<Option<Json>> {
        self.invocation.check()?;
        let state = lock(&self.scheduler.state);
        let record = state.live.get(&self.invocation.task_id);
        Ok(record.and_then(|record| record.memos.as_ref()).and_then(|memos| memos.get(name)).cloned())
    }

    /// Store `candidate` unless a memo already exists; return the durable winner.
    pub async fn memo_or(&self, name: &str, candidate: Json, context: &Context) -> DurableResult<Json> {
        let name = name.to_owned();
        self.gated(
            move |tx, current| async move {
                if let Some(winner) = current.memos.as_ref().and_then(|memos| memos.get(&name)) {
                    return Ok(winner.clone());
                }
                let mut memos = current.memos.clone().unwrap_or_default();
                memos.insert(name, candidate.clone());
                tx.set_task(TaskRecord { memos: Some(memos), ..current })?;
                Ok(candidate)
            },
            context,
        )
        .await
    }

    /// Committed task record.
    pub async fn get_task(&self, id: TaskId, context: &Context) -> DurableResult<Option<TaskRecord>> {
        self.invocation.check()?;
        let (storage, context) = (self.scheduler.core.session.storage(), context.clone());
        self.scheduler.core.session.read_on_line(async move { storage.task(id, &context).await }).await
    }

    /// Resolve with the task's terminal receipt; fails when the invocation ends.
    pub async fn wait_for_task(&self, id: TaskId, context: &Context) -> DurableResult<TaskRecord> {
        self.invocation.check()?;
        let bound = with_abort_signal(&self.signal(), context);
        self.scheduler.wait_for_task(id, &bound).await
    }

    /// Abort a task this task owns and resolve once it is terminal.
    pub async fn abort_owned(&self, id: TaskId, context: &Context) -> DurableResult<()> {
        self.invocation.check()?;
        let bound = with_abort_signal(&self.signal(), context);
        let record = self.get_task(id, &bound).await?;
        let owner = record.as_ref().and_then(|record| record.owner);
        if owner != Some(self.invocation.task_id) {
            let message = format!("Task {id} is not owned by task {}", self.invocation.task_id);
            return Err(DurableError::error(message));
        }
        if record.is_some_and(|record| record.is_terminal()) {
            return Ok(());
        }
        if self.scheduler.abort(id, &bound, true).await? == AbortTaskResult::Terminal {
            return Ok(());
        }
        self.scheduler.wait_for_task(id, &bound).await.map(|_| ())
    }

    /// Outcomes of terminal tasks, in order; fails when one is missing or not terminal.
    pub async fn outcomes(&self, ids: &[TaskId], context: &Context) -> DurableResult<Vec<TaskOutcome>> {
        self.invocation.check()?;
        let (storage, context, ids) = (self.scheduler.core.session.storage(), context.clone(), ids.to_vec());
        let read = async move {
            let mut outcomes = Vec::new();
            for id in ids {
                let record = storage.task(id, &context).await?.filter(|record| record.is_terminal());
                let Some(outcome) = record.and_then(|record| record.state.outcome().cloned()) else {
                    return Err(DurableError::error(format!("Task {id} is not terminal")));
                };
                outcomes.push(outcome);
            }
            Ok(outcomes)
        };
        self.scheduler.core.session.read_on_line(read).await
    }

    /// Invocation-bound handle of an existing conversation; `None` when absent.
    pub async fn conversation(&self, id: ConversationId, context: &Context) -> DurableResult<Option<ConversationHandle>> {
        self.invocation.check()?;
        let (storage, read_context) = (self.scheduler.core.session.storage(), context.clone());
        let record =
            self.scheduler.core.session.read_on_line(async move { storage.conversation(id, &read_context).await }).await?;
        Ok(record.map(|_| ConversationHandle { id, invocation: self.invocation.clone(), scheduler: self.scheduler.clone() }))
    }

    /// Committed entry visible from the task's conversation.
    pub async fn entry(&self, id: EntryId, context: &Context) -> DurableResult<Option<EntryRecord>> {
        self.invocation.check()?;
        let (storage, context, conversation) =
            (self.scheduler.core.session.storage(), context.clone(), self.invocation.conversation_id);
        let read = async move { storage.visible_entry(conversation, id, &context).await };
        Ok(self.scheduler.core.session.read_on_line(read).await?.map(|stored| stored.entry))
    }

    /// `None` when the entry is absent, not visible, or has another kind.
    pub async fn typed_entry<D>(&self, token: &Entry<D>, id: EntryId, context: &Context) -> DurableResult<Option<TypedEntry<D>>>
    where
        D: serde::Serialize + serde::de::DeserializeOwned,
    {
        match self.entry(id, context).await? {
            Some(entry) => token.decode(entry),
            None => Ok(None),
        }
    }

    /// Committed raw active transcript and model context, optionally cut off at the visible entry `at`.
    pub async fn context(&self, conversation_id: ConversationId, context: &Context, at: Option<EntryId>) -> DurableResult<ContextView> {
        self.invocation.check()?;
        let scheduler = &self.scheduler;
        let kept = lock(&scheduler.state).contexts.get(&conversation_id).map(|kept| kept.range.clone());
        let (view, range) = read_context_from(&scheduler.core.session, conversation_id, context, at, kept).await?;
        if let Some(range) = range {
            let retention = scheduler.core.settings().context_retention_ms;
            let now = scheduler.core.now();
            let mut expire = false;
            {
                let mut state = lock(&scheduler.state);
                let kept = state.contexts.get(&conversation_id);
                let newer = kept.is_none_or(|kept| kept.range.bounds.tail <= range.bounds.tail);
                if newer && !state.closing && !self.invocation.is_ended() {
                    let idle_since = kept.and_then(|kept| kept.idle_since);
                    if !state.idle(Some(conversation_id)) {
                        state.contexts.insert(conversation_id, KeptContext { range, idle_since: None });
                    } else if retention > 0.0 {
                        let idle_since = Some(idle_since.unwrap_or(now));
                        state.contexts.insert(conversation_id, KeptContext { range, idle_since });
                        expire = true;
                    }
                }
            }
            if expire {
                scheduler.schedule_expiry();
            }
        }
        Ok(view)
    }

    /// The Harness clock.
    pub fn now(&self) -> f64 {
        self.scheduler.core.now()
    }

    /// Forward a non-fatal failure to `HarnessOptions::on_report`.
    pub fn report(&self, error: &DurableError) {
        self.scheduler.core.report(error);
    }

    /// Resolve once the Harness clock reaches `until`; fails when the invocation or `context` is cancelled.
    pub async fn sleep(&self, until: f64, context: &Context) -> DurableResult<()> {
        self.invocation.check()?;
        let bound = with_abort_signal(&self.signal(), context);
        loop {
            if bound.is_cancelled() {
                return Err(crate::harness::util::cancellation(&bound));
            }
            let remaining = until - self.now();
            if remaining <= 0.0 {
                return Ok(());
            }
            let wait = remaining.min(super::reconcile::MAX_TIMER_DELAY_MS) as u64;
            let sleep = async move {
                tokio::time::sleep(std::time::Duration::from_millis(wait.max(1))).await;
                Ok(())
            };
            with_context(sleep, &bound).await?;
        }
    }

    /// Snapshot of a committed document.
    pub async fn snapshot<T: DocValue, S: DocScope>(
        &self,
        token: &DocToken<T, S>,
        owner: S::Owner,
        context: &Context,
    ) -> DurableResult<Option<Json>> {
        self.invocation.check()?;
        self.scheduler.core.session.snapshot(token, owner, context).await
    }

    pub async fn family_snapshot<T: DocValue, I: DocValue, S: DocScope>(
        &self,
        token: &DocFamilyToken<T, I, S>,
        owner: S::Owner,
        key: &str,
        context: &Context,
    ) -> DurableResult<Option<Json>> {
        self.invocation.check()?;
        self.scheduler.core.session.family_snapshot(token, owner, key, context).await
    }

    pub async fn snapshot_as_of<T: DocValue, S: RewindableDocScope>(
        &self,
        token: &DocToken<T, S>,
        conversation_id: ConversationId,
        at: EntryId,
        context: &Context,
    ) -> DurableResult<Option<Json>> {
        self.invocation.check()?;
        self.scheduler.core.session.snapshot_as_of(token, conversation_id, at, context).await
    }

    /// A watch of a committed document; it stops when the invocation ends.
    pub async fn watch_doc<T: DocValue, S: DocScope>(
        &self,
        token: &DocToken<T, S>,
        owner: S::Owner,
        context: &Context,
    ) -> DurableResult<Option<CommittedWatch>> {
        self.invocation.check()?;
        let Some(watch) = self.scheduler.core.session.watch_doc(token, owner, context).await? else { return Ok(None) };
        if self.invocation.is_ended() {
            drop(watch.stop());
            return Err(self.invocation.ended_error());
        }
        lock(&self.invocation.watches).push(watch.clone());
        Ok(Some(watch))
    }
}

/// Invocation-bound conversation operations for tasks and tools; they fail after the invocation ends.
#[derive(Clone)]
pub struct ConversationHandle {
    pub id: ConversationId,
    invocation: Arc<Invocation>,
    scheduler: Arc<Scheduler>,
}

impl ConversationHandle {
    fn bind(&self, context: &Context) -> DurableResult<Context> {
        self.invocation.check()?;
        Ok(with_abort_signal(&self.invocation.controller.signal(), context))
    }

    /// Admit input; the returned submission's operations are bound to the invocation too.
    pub async fn submit(&self, draft: SubmissionDraft, context: &Context) -> DurableResult<Submission> {
        if matches!(draft, SubmissionDraft::Write { .. }) {
            return Err(DurableError::type_error("Task conversation handles submit input only"));
        }
        let bound = self.bind(context)?;
        let submissions = self.scheduler.submissions.get().and_then(std::sync::Weak::upgrade);
        let submissions = submissions.ok_or_else(|| crate::harness::util::closed_error(&self.scheduler.core.session))?;
        let submission = submissions.submit(self.id, draft, &bound).await?;
        Ok(submission.bound(self.invocation.clone()))
    }

    /// Withdraw queued inputs, abort the ordinary ownership scope, and wait until it is idle.
    pub async fn abort(&self, context: &Context, background: bool) -> DurableResult<()> {
        let bound = self.bind(context)?;
        self.scheduler.abort_conversation(self.id, background, &bound).await
    }

    /// Resolve when the conversation's ordinary ownership scope has no live non-background task.
    pub async fn wait_for_idle(&self, context: &Context) -> DurableResult<()> {
        let bound = self.bind(context)?;
        self.scheduler.wait_for_idle(Some(self.id), &bound).await
    }
}
