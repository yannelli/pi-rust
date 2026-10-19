//! Invocations: phase handlers and the step before each, the abort handler, and the outcome and wait writes.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use pi_chord::Json;

use super::ownership::{Overlay, Step};
use super::run::Reservation;
use super::runtime::{Phase, TaskRuntime};
use super::{Invocation, Mode, Scheduler, Up, with_state};
use crate::errors::{DurableError, DurableResult};
use crate::harness::live::settle_scheduler_outcome;
use crate::harness::registry::RegistrySnapshot;
use crate::harness::task::{Task, phase_of};
use crate::harness::util::lock;
use crate::ids::TaskId;
use crate::session::{TransactionScope, Tx};
use crate::types::{JoinPolicy, TaskOutcome, TaskOutcomeError, TaskRecord, TaskState, TaskStatus};

/// What a running invocation tracks across steps: its definition, its snapshot, and the last reported takeover.
pub(crate) struct RunState {
    pub task: Task,
    pub snapshot: RegistrySnapshot,
    reported: Option<Option<Task>>,
}

/// Outcome of the phase that just returned, judged by the next step.
struct PhaseResult {
    checkpoint: Json,
    failure: Option<DurableError>,
}

/// Step decision: continue with the next phase, end the invocation, or end it by writing `faulted`.
enum Decision {
    Continue,
    End,
    Fault(DurableError),
}

fn same(a: &Option<Task>, b: &Option<Task>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

impl Scheduler {
    pub(super) fn start(&self, reservation: Reservation) {
        let Ok(scheduler) = self.arc() else { return };
        tokio::spawn(async move {
            let invocation = reservation.invocation.clone();
            let result = match invocation.mode {
                Mode::Run => scheduler.run(reservation).await,
                Mode::Abort => scheduler.run_abort(reservation).await,
            };
            if let Err(error) = result {
                scheduler.core.report(&error);
            }
            scheduler.end(&invocation);
            invocation.done.send_replace(true);
            scheduler.kick();
        });
    }

    /// Run phase handlers, each preceded by a step that decides on the line whether the invocation continues.
    async fn run(&self, reservation: Reservation) -> DurableResult<()> {
        let invocation = reservation.invocation;
        let state = RunState { task: reservation.task, snapshot: reservation.snapshot, reported: None };
        let state = Arc::new(Mutex::new(state));
        let phase = Arc::new(Phase::new(state.clone()));
        let runtime = TaskRuntime::new(self.arc()?, invocation.clone(), phase.clone());
        let mut previous: Option<PhaseResult> = None;
        loop {
            let scheduler = self.arc()?;
            let (run_state, last) = (state.clone(), previous.take());
            let decide = move |tx: &Tx, current: &TaskRecord| scheduler.decide(tx, current, last, &run_state);
            let Some(current) = self.step(&invocation, decide).await else { return Ok(()) };
            // Close may seal between the decision and dispatch.
            if self.closing() {
                return Ok(());
            }
            let checkpoint = current.state.checkpoint().cloned().unwrap_or(Json::Null);
            phase.reset_agent();
            let task = lock(&state).task.clone();
            let name = phase_of(&checkpoint);
            let result = match task.phase_handler(&name) {
                Some(handler) => handler(current, runtime.clone(), invocation.context.clone()).await,
                None => Err(DurableError::type_error(format!("Task {} has no phase {name}", task.name))),
            };
            previous = Some(PhaseResult { checkpoint, failure: result.err() });
        }
    }

    /// Precedence rules for a run invocation, on the line.
    fn decide(
        &self,
        tx: &Tx,
        current: &TaskRecord,
        previous: Option<PhaseResult>,
        state: &Mutex<RunState>,
    ) -> DurableResult<Decision> {
        // 3. abort mark: end; a fresh abort invocation starts once the owned work is gone.
        if current.abort_requested {
            return Ok(Decision::End);
        }
        let below = !current.background && lock(&self.state).below_cancelled(Up::of(current));
        if below {
            lock(&self.state).cascade_pending = true;
            self.schedule_reconcile();
            return Ok(Decision::End);
        }
        let Some(previous) = previous else { return Ok(Decision::Continue) };
        // 4. uncaught error.
        if let Some(error) = previous.failure {
            return Ok(Decision::Fault(error));
        }
        // 6. no durable progress.
        if current.state.checkpoint() == Some(&previous.checkpoint) {
            let phase = phase_of(&previous.checkpoint);
            let message = format!("Task {} phase {phase} returned without durable progress", current.kind);
            return Ok(Decision::Fault(DurableError::error(message)));
        }
        // 5. progress: refresh the snapshot; hand over to a replacement definition that can take the task.
        let snapshot = self.core.registry.snapshot();
        let next = snapshot.task(&current.kind);
        let mut run = lock(state);
        run.snapshot = snapshot;
        if next.as_ref().is_some_and(|next| Arc::ptr_eq(next, &run.task)) {
            return Ok(Decision::Continue);
        }
        if let Some(next) = &next
            && can_reserve(next, current)
        {
            let checkpoint = current.state.checkpoint().cloned().unwrap_or(Json::Null);
            tx.set_task(with_state(current, TaskState::Pending { checkpoint }))?;
            return Ok(Decision::End);
        }
        if !run.reported.as_ref().is_some_and(|task| same(task, &next)) {
            run.reported = Some(next);
            let message = format!("Task {} keeps running under its old {} definition", current.id, current.kind);
            self.core.report(&DurableError::error(message));
        }
        Ok(Decision::Continue)
    }

    /// Run the abort handler once; returning without an outcome faults.
    async fn run_abort(&self, reservation: Reservation) -> DurableResult<()> {
        let invocation = reservation.invocation;
        let current = lock(&self.state).live.get(&invocation.task_id).cloned();
        let Some(current) = current else { return Ok(()) };
        if self.closing() {
            return Ok(());
        }
        let task = reservation.task;
        let state = RunState { task: task.clone(), snapshot: reservation.snapshot, reported: None };
        let phase = Arc::new(Phase::new(Arc::new(Mutex::new(state))));
        let runtime = TaskRuntime::new(self.arc()?, invocation.clone(), phase);
        let failure = match task.abort_handler() {
            Some(handler) => handler(current, runtime, invocation.context.clone()).await.err(),
            None => Some(DurableError::type_error(format!("Task {} has no abort handler", task.name))),
        };
        let message = format!("Abort handler of task {} returned without a terminal outcome", invocation.task_id);
        let fault = failure.unwrap_or_else(|| DurableError::error(message));
        self.step(&invocation, move |_, _| Ok(Decision::Fault(fault))).await;
        Ok(())
    }

    /// One decision on the Session line. A task no longer running or a closing Harness ends the invocation.
    async fn step<F>(&self, invocation: &Arc<Invocation>, decide: F) -> Option<TaskRecord>
    where
        F: FnOnce(&Tx, &TaskRecord) -> DurableResult<Decision> + Send + 'static,
    {
        let Ok(scheduler) = self.arc() else { return None };
        let target = invocation.clone();
        let committed = self.core.session.commit_with(
            move |tx| async move {
                let current = {
                    let state = lock(&scheduler.state);
                    let found = state.live.get(&target.task_id).filter(|r| r.status() == TaskStatus::Running);
                    found.cloned().filter(|_| !state.closing)
                };
                let decision = match &current {
                    Some(current) => decide(&tx, current)?,
                    None => Decision::End,
                };
                if let Decision::Continue = decision {
                    return Ok(current);
                }
                scheduler.end(&target);
                if let (Decision::Fault(error), Some(current)) = (decision, current) {
                    let outcome = TaskOutcome::Faulted { error: TaskOutcomeError::new(error.to_string()) };
                    scheduler.terminate(&tx, &current, outcome).await?;
                }
                Ok(None)
            },
            &self.context,
            TransactionScope::default(),
        );
        match committed.await {
            Ok(current) => current,
            Err(error) => {
                // A failed step would leave the task running, to be reserved and run again.
                self.end(invocation);
                self.fail_session(error);
                None
            }
        }
    }

    /// Write an outcome the scheduler decided: `completing` while owned work is live, else terminal with cleanup.
    pub(crate) async fn terminate(&self, tx: &Tx, record: &TaskRecord, outcome: TaskOutcome) -> DurableResult<()> {
        self.load_scopes(false).await?;
        let owned = lock(&self.state).owned_live(Some(&Overlay::of(tx))).contains_key(&record.id);
        if owned {
            return tx.set_task(with_state(record, TaskState::Completing { outcome }));
        }
        tx.set_task(with_state(record, TaskState::Terminal { outcome: outcome.clone() }))?;
        settle_scheduler_outcome(tx, record, &outcome).await
    }

    /// Replace a running task's state with what it committed; a terminal state holds as `completing` while
    /// ordinary owned work is live. A wait is validated first.
    pub(crate) async fn commit_state(
        &self,
        tx: &Tx,
        invocation: &Invocation,
        current: &TaskRecord,
        next: TaskState,
    ) -> DurableResult<()> {
        match &next {
            TaskState::Waiting { on, policy, .. } => self.validate_wait(tx, invocation, current, on, *policy).await?,
            TaskState::Sleeping { until, .. } => validate_sleep(invocation, current.id, *until)?,
            TaskState::Terminal { outcome } => {
                let overlay = Overlay::of(tx);
                self.load_scopes(false).await?;
                for record in overlay.tasks.values() {
                    self.load_chain(Up::of(record), Some(&overlay)).await?;
                }
                if lock(&self.state).owned_live(Some(&overlay)).contains_key(&current.id) {
                    let completing = TaskState::Completing { outcome: outcome.clone() };
                    return tx.set_task(with_state(current, completing));
                }
            }
            TaskState::Running { .. } => {}
            other => {
                let status = other.status().as_str();
                return Err(DurableError::type_error(format!("A task cannot commit the state {status}")));
            }
        }
        tx.set_task(with_state(current, next))
    }

    /// A wait names existing tasks other than the waiter and its owners; `failFast` only tasks it owns.
    async fn validate_wait(
        &self,
        tx: &Tx,
        invocation: &Invocation,
        current: &TaskRecord,
        on: &[TaskId],
        policy: JoinPolicy,
    ) -> DurableResult<()> {
        if invocation.mode == Mode::Abort {
            return Err(DurableError::error(format!("Abort handler of task {} cannot wait", current.id)));
        }
        let overlay = Overlay::of(tx);
        self.load_chain(Up::of(current), None).await?;
        let owners: Vec<TaskId> = lock(&self.state)
            .above(Up::of(current), None)
            .into_iter()
            .filter_map(|step| match step {
                Step::Task(id, _) => Some(id),
                _ => None,
            })
            .collect();
        for id in on {
            if *id == current.id || owners.contains(id) {
                return Err(DurableError::error(format!(
                    "Task {} cannot wait on itself or its owner {id}",
                    current.id
                )));
            }
            let known = overlay.tasks.get(id).cloned().or_else(|| lock(&self.state).live.get(id).cloned());
            let member = match known {
                Some(member) => Some(member),
                None => self.core.session.storage().task(*id, &self.context).await?,
            };
            let Some(member) = member else { return Err(DurableError::error(format!("Task {id} does not exist"))) };
            if policy == JoinPolicy::FailFast && member.owner != Some(current.id) {
                let message = format!("Task {} can wait failFast only on tasks it owns; {id} is not one", current.id);
                return Err(DurableError::error(message));
            }
        }
        Ok(())
    }

    /// End an invocation: its runtime operations fail from now on, its signal aborts, its watches stop.
    pub(crate) fn end(&self, invocation: &Arc<Invocation>) {
        if invocation.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut state = lock(&self.state);
            if state.invocations.get(&invocation.task_id).is_some_and(|i| Arc::ptr_eq(i, invocation)) {
                state.invocations.remove(&invocation.task_id);
            }
        }
        let watches = std::mem::take(&mut *lock(&invocation.watches));
        for watch in watches {
            drop(watch.stop());
        }
        let timers = std::mem::take(&mut *lock(&invocation.timers));
        for disarm in timers.into_values() {
            disarm();
        }
        invocation.controller.abort(Some(invocation.ended_error().into()));
    }
}

/// A sleep's `until` is a time on the Harness clock; an abort handler cannot sleep.
fn validate_sleep(invocation: &Invocation, task: TaskId, until: f64) -> DurableResult<()> {
    if invocation.mode == Mode::Abort {
        return Err(DurableError::error(format!("Abort handler of task {task} cannot sleep")));
    }
    if !until.is_finite() || until < 0.0 {
        let until = pi_chord::json::format_js_number(until);
        return Err(DurableError::error(format!("Task {task} cannot sleep until {until}: not a time in milliseconds")));
    }
    Ok(())
}

/// Whether a definition can take the task at reservation: same version, or newer with a migration.
fn can_reserve(task: &Task, record: &TaskRecord) -> bool {
    task.version == record.version || (task.version > record.version && task.has_migrate())
}
