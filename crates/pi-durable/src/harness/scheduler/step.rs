//! Invocations: phase handlers and the step before each, the abort handler, and the outcome and wait writes.

use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pi_chord::{Context, Json, with_telemetry, without_abort_signal};
use pi_telemetry::{SpanAttributes, SpanLink, SpanOptions, SpanStatus, TelemetryContextExt, TelemetrySpan};

use super::ownership::{Overlay, Step};
use super::run::Reservation;
use super::runtime::{Phase, TaskRuntime};
use super::{Invocation, Mode, Scheduler, Up, progressed, report_task, with_state};
use crate::errors::{DurableError, DurableResult};
use crate::harness::live::settle_scheduler_outcome;
use crate::harness::registry::RegistrySnapshot;
use crate::harness::task::{Task, phase_of};
use crate::harness::util::lock;
use crate::ids::TaskId;
use crate::session::{TransactionScope, Tx};
use crate::types::{
    FaultReason, JoinPolicy, OutdatedReason, ReportInfo, TaskOutcome, TaskOutcomeError, TaskRecord, TaskState,
    TaskStatus,
};

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
                let info =
                    ReportInfo::TaskError { task: invocation.report_task(), extension: invocation.extension.clone() };
                scheduler.core.report(&error, &info, Some(&invocation.context()));
            }
            scheduler.end(&invocation);
            invocation.done.send_replace(true);
            scheduler.kick();
        });
    }

    /// Run phase handlers, each followed by a step that decides on the line whether the invocation continues.
    async fn run(&self, reservation: Reservation) -> DurableResult<()> {
        let invocation = reservation.invocation;
        let state = RunState { task: reservation.task, snapshot: reservation.snapshot, reported: None };
        let state = Arc::new(Mutex::new(state));
        let phase = Arc::new(Phase::new(state.clone()));
        let runtime = TaskRuntime::new(self.arc()?, invocation.clone(), phase.clone());
        let mut current = self.step(&invocation, self.decider(&state, &invocation, None)?).await;
        while let Some(task) = current {
            // Close may seal between the decision and dispatch.
            if self.closing() {
                return Ok(());
            }
            let checkpoint = task.state.checkpoint().cloned().unwrap_or(Json::Null);
            phase.reset_agent();
            let definition = lock(&state).task.clone();
            let name = phase_of(&checkpoint);
            let record = task.clone();
            let (state, runtime, invocation) = (&state, &runtime, &invocation);
            let handled = async move {
                let result = match definition.phase_handler(&name) {
                    Some(handler) => handler(task.clone(), runtime.clone(), invocation.context()).await,
                    None => Err(DurableError::type_error(format!("Task {} has no phase {name}", definition.name))),
                };
                let previous = PhaseResult { checkpoint, failure: result.err() };
                Ok::<_, DurableError>(self.step(invocation, self.decider(state, invocation, Some(previous))?).await)
            };
            current = self.phase(invocation, &record, handled).await?;
        }
        Ok(())
    }

    /// The decision of the step after `previous`, or before the first phase.
    fn decider(
        &self,
        state: &Arc<Mutex<RunState>>,
        invocation: &Invocation,
        previous: Option<PhaseResult>,
    ) -> DurableResult<impl FnOnce(&Tx, &TaskRecord) -> DurableResult<Decision> + Send + 'static> {
        let scheduler = self.arc()?;
        let (state, extension, context) = (state.clone(), invocation.extension.clone(), invocation.context());
        Ok(move |tx: &Tx, current: &TaskRecord| scheduler.decide(tx, current, previous, &state, extension, &context))
    }

    /// Run one phase, its handler and its decision commit, in a `pi.durable.phase` span. The span's parent is the
    /// task's stored carrier, else the adapter root; the handler's context carries the span.
    async fn phase<T>(&self, invocation: &Invocation, task: &TaskRecord, run: impl Future<Output = T>) -> T {
        let telemetry = &self.core.telemetry;
        let parent = match (&task.trace, telemetry.root.propagation()) {
            (Some(trace), Some(propagation)) => propagation.extract(Some(trace)),
            _ => telemetry.root.clone(),
        };
        let checkpoint = task.state.checkpoint().cloned().unwrap_or(Json::Null);
        let name = phase_of(&checkpoint);
        let resumed = lock(&self.state).resumed.remove(&task.id);
        let attributes = SpanAttributes::new()
            .with("task.id", task.id.get())
            .with("task.name", task.kind.as_str())
            .with("phase", name.as_str())
            .with("kind", if invocation.mode == Mode::Run { "run" } else { "abort" })
            .with("resumed", resumed)
            .with_optional("attempt", checkpoint.get("attempt").and_then(Json::as_f64));
        let options = SpanOptions::new("pi.durable.phase").with_attributes(attributes);
        parent
            .start_infallible_span(options, |span| async move {
                if let Some(propagation) = span.propagation() {
                    for carrier in &invocation.links {
                        span.add_link(SpanLink::new(propagation.extract(Some(carrier))));
                    }
                }
                let previous = invocation.context();
                *lock(&invocation.context) = with_telemetry(span.clone(), &previous);
                let started = Instant::now();
                let result = run.await;
                *lock(&invocation.context) = previous;
                let outcome = self.phase_outcome(invocation, &span);
                let attributes =
                    SpanAttributes::new().with("task", task.kind.as_str()).with("phase", name).with("outcome", outcome);
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                let _ = telemetry.duration.record(elapsed, attributes, Some(span.as_ref()));
                result
            })
            .await
    }

    /// The phase span's `outcome` and status from the task's state after the phase: failed outcomes are errors with a
    /// fixed name and a diagnostic capped at 500 characters; an aborted one stays `ok`.
    fn phase_outcome(&self, invocation: &Invocation, span: &Arc<dyn TelemetrySpan>) -> String {
        let record = lock(&invocation.record).clone();
        let outcome = match record.state.outcome() {
            Some(TaskOutcome::Completed { .. }) => "done".to_owned(),
            Some(TaskOutcome::Aborted { .. }) => "aborted".to_owned(),
            Some(_) => "faulted".to_owned(),
            None => match &record.state {
                TaskState::Waiting { .. } => "waiting".to_owned(),
                TaskState::Sleeping { .. } => "sleeping".to_owned(),
                _ if self.closing() => "aborted".to_owned(),
                state => format!("next={}", phase_of(state.checkpoint().unwrap_or(&Json::Null))),
            },
        };
        span.set_attributes(SpanAttributes::new().with("outcome", outcome.as_str()));
        let failure = match record.state.outcome() {
            Some(TaskOutcome::Faulted { error }) => Some(("task_faulted", error.message.as_str())),
            Some(TaskOutcome::Failed { error, .. }) => Some(("task_failed", error.message.as_str())),
            Some(TaskOutcome::Orphaned { reason }) => Some(("task_orphaned", reason.as_str())),
            _ => None,
        };
        if let Some((name, message)) = failure {
            span.set_status(SpanStatus::error(name, message.chars().take(500).collect::<String>()));
        } else if outcome == "aborted" {
            span.set_status(SpanStatus::Ok);
        }
        outcome
    }

    /// Precedence rules for a run invocation, on the line.
    fn decide(
        &self,
        tx: &Tx,
        current: &TaskRecord,
        previous: Option<PhaseResult>,
        state: &Mutex<RunState>,
        extension: Option<String>,
        context: &Context,
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
            let reason = if next.is_none() { OutdatedReason::MissingTask } else { OutdatedReason::IncompatibleTask };
            run.reported = Some(next);
            let message = format!("Task {} keeps running under its old {} definition", current.id, current.kind);
            let info = ReportInfo::TaskOutdated { task: report_task(current), extension, reason };
            self.core.report(&DurableError::error(message), &info, Some(context));
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
        let handled = async {
            let failure = match task.abort_handler() {
                Some(handler) => handler(current.clone(), runtime, invocation.context()).await.err(),
                None => Some(DurableError::type_error(format!("Task {} has no abort handler", task.name))),
            };
            let message = format!("Abort handler of task {} returned without a terminal outcome", invocation.task_id);
            let fault = failure.unwrap_or_else(|| DurableError::error(message));
            self.step(&invocation, move |_, _| Ok(Decision::Fault(fault))).await;
        };
        self.phase(&invocation, &current, handled).await;
        Ok(())
    }

    /// One decision on the Session line. A task no longer running or a closing Harness ends the invocation.
    async fn step<F>(&self, invocation: &Arc<Invocation>, decide: F) -> Option<TaskRecord>
    where
        F: FnOnce(&Tx, &TaskRecord) -> DurableResult<Decision> + Send + 'static,
    {
        let Ok(scheduler) = self.arc() else { return None };
        let target = invocation.clone();
        let fault: Arc<Mutex<Option<DurableError>>> = Arc::default();
        let faulted = fault.clone();
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
                    *lock(&faulted) = Some(error);
                }
                // The ended invocation no longer hears this commit's record, which its phase span reports.
                if let Some(staged) = tx.staged_tasks().into_iter().find(|record| record.id == target.task_id) {
                    *lock(&target.record) = staged;
                }
                Ok(None)
            },
            &without_abort_signal(&invocation.context()),
            TransactionScope::default(),
        );
        match committed.await {
            Ok(current) => {
                // Reported once the fault is durable: a host may count it, for example toward a breaker.
                if let Some(error) = lock(&fault).take() {
                    let info = ReportInfo::TaskFaulted {
                        task: invocation.report_task(),
                        extension: invocation.extension.clone(),
                        reason: FaultReason::Threw,
                    };
                    self.core.report(&error, &info, Some(&invocation.context()));
                }
                current
            }
            Err(error) => {
                // A failed step would leave the task running, to be reserved and run again.
                self.end(invocation);
                self.fail_session(error, Some(&invocation.context()));
                None
            }
        }
    }

    /// Write an outcome the scheduler decided: `completing` while owned work is live, else terminal with cleanup.
    pub(crate) async fn terminate(&self, tx: &Tx, record: &TaskRecord, outcome: TaskOutcome) -> DurableResult<()> {
        self.load_scopes(false).await?;
        let owned = lock(&self.state).has_owned_live(record.id, Some(&Overlay::of(tx)), None);
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
                if lock(&self.state).has_owned_live(current.id, Some(&overlay), None) {
                    let completing = TaskState::Completing { outcome: outcome.clone() };
                    return tx.set_task(with_state(&progressed(current), completing));
                }
            }
            TaskState::Running { .. } => {}
            other => {
                let status = other.status().as_str();
                return Err(DurableError::type_error(format!("A task cannot commit the state {status}")));
            }
        }
        // A state the task committed that differs from its current one is progress: its restart count starts over.
        // The same checkpoint committed again, for example as intent before an effect, is not.
        if current.state == next {
            return tx.set_task(with_state(current, next));
        }
        tx.set_task(with_state(&progressed(current), next))
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
                if state.live.contains_key(&invocation.task_id) {
                    state.runnable.insert(invocation.task_id);
                }
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
