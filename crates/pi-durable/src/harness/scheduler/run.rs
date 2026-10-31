//! Reservation of eligible tasks, definition resolution and migration, inspection, and invocation creation.

use std::sync::{Arc, Mutex};

use pi_chord::{AbortController, Json, with_abort_signal};
use tokio::sync::watch;

use super::{Invocation, Mode, SchedState, Scheduler, Up, installer_of, report_task, with_abort_mark, with_state};
use crate::errors::{DurableError, DurableResult};
use crate::harness::registry::RegistrySnapshot;
use crate::harness::task::{Task, missing_migration};
use crate::harness::types::{BlockedReason, Scheduling, TaskInspection, TaskInspectionState};
use crate::harness::util::lock;
use crate::ids::TaskId;
use crate::session::{TransactionScope, Tx};
use crate::types::{
    AbortReason, FaultReason, ReportInfo, TaskOutcome, TaskOutcomeError, TaskRecord, TaskState, TaskStatus,
};

/// What a reservation pass leaves to do once its commit lands.
#[derive(Default)]
struct Pass {
    /// Abandoned tasks the pass abort-marked.
    abandoned: Vec<TaskId>,
    /// Abort-marked tasks faulted by the restart limit, with their messages, to report.
    faulted: Vec<(TaskRecord, String)>,
}

pub(crate) struct Reservation {
    pub invocation: Arc<Invocation>,
    pub task: Task,
    pub snapshot: RegistrySnapshot,
}

impl Scheduler {
    /// Reserve every eligible task in one commit; orphan abort-marked tasks no definition can take.
    pub(super) async fn reserve(&self) -> DurableResult<Vec<Reservation>> {
        let scheduler = self.arc()?;
        // Invocations reserved by a transaction whose commit then fails never start, so they are forgotten here.
        let pending: Arc<Mutex<Vec<Arc<Invocation>>>> = Arc::default();
        let recorded = pending.clone();
        let reserved = self.core.session.commit_with(
            move |tx| async move {
                let mut reservations = Vec::new();
                match scheduler.reserve_in(&tx, &mut reservations).await {
                    Ok(pass) => {
                        *lock(&recorded) = reservations.iter().map(|r| r.invocation.clone()).collect();
                        Ok((reservations, pass))
                    }
                    Err(error) => {
                        for reservation in reservations {
                            scheduler.forget(&reservation.invocation);
                        }
                        Err(error)
                    }
                }
            },
            &self.context,
            TransactionScope::default(),
        );
        let (reservations, pass): (Vec<Reservation>, Pass) = match reserved.await {
            Ok(reserved) => reserved,
            Err(error) => {
                for invocation in std::mem::take(&mut *lock(&pending)) {
                    self.forget(&invocation);
                }
                return Err(error);
            }
        };
        for (record, message) in pass.faulted {
            let (task, extension) = self.about_task(&record);
            let info = ReportInfo::TaskFaulted { task, extension, reason: FaultReason::RestartLimit };
            self.core.report(&DurableError::error(message), &info, None);
        }
        if !pass.abandoned.is_empty() {
            let mut state = lock(&self.state);
            for id in &pass.abandoned {
                state.abandoned.remove(id);
            }
            state.dirty = true;
        }
        Ok(reservations)
    }

    fn forget(&self, invocation: &Arc<Invocation>) {
        {
            let mut state = lock(&self.state);
            if state.invocations.get(&invocation.task_id).is_some_and(|i| Arc::ptr_eq(i, invocation)) {
                state.invocations.remove(&invocation.task_id);
                state.runnable.insert(invocation.task_id);
            }
        }
        invocation.done.send_replace(true);
    }

    async fn reserve_in(&self, tx: &Tx, reservations: &mut Vec<Reservation>) -> DurableResult<Pass> {
        let mut pass = Pass::default();
        {
            let state = lock(&self.state);
            if !state.enabled || state.closing {
                return Ok(pass);
            }
            if !state.abandoned.is_empty() {
                for id in &state.abandoned {
                    if let Some(record) = state.live.get(id)
                        && !record.abort_requested
                    {
                        tx.set_task(with_abort_mark(record, Some(AbortReason::Restart)))?;
                    }
                }
                pass.abandoned = state.abandoned.iter().copied().collect();
                return Ok(pass);
            }
        }
        self.load_scopes(false).await?;
        let now = self.core.now();
        {
            // Every pass picks up sleeps whose time has come: a pass started by any change may be the first to run after
            // a deadline, and the wake it schedules no longer keeps a timer for a due sleep.
            let mut state = lock(&self.state);
            state.wake_due(now);
        }
        let candidates: Vec<TaskId> = lock(&self.state).runnable.iter().copied().collect();
        let mut snapshot: Option<RegistrySnapshot> = None;
        for id in candidates {
            let (record, below) = {
                let mut state = lock(&self.state);
                let Some(record) = state.live.get(&id).cloned() else {
                    state.runnable.remove(&id);
                    continue;
                };
                // It returns once what it waits for ends, its record changes, or the work below it ends.
                let busy = state.invocations.contains_key(&id)
                    || record.status() == TaskStatus::Completing
                    || !waiting_on(&state, &record, true).is_empty()
                    // A sleep returns at the first pass after its time comes, which the timer, `resume()`, or any change
                    // starts.
                    || not_due(&record, now);
                if busy {
                    state.runnable.remove(&id);
                    continue;
                }
                let below = !record.abort_requested && !record.background && state.below_cancelled(Up::of(&record));
                (record, below)
            };
            let restarts = record.restarts.unwrap_or(0);
            let mode = if record.abort_requested { Mode::Abort } else { Mode::Run };
            if below {
                // Work below an owner with cancellation intent waits for its cascade mark instead of running.
                lock(&self.state).cascade_pending = true;
                self.schedule_reconcile();
                continue;
            }
            let cut_off = record.restarts.is_some_and(|count| f64::from(count) >= self.restart_limit());
            if cut_off && (mode == Mode::Abort || !crate::harness::live::limits_restarts(&record.kind)) {
                let message = if mode == Mode::Abort {
                    format!("Abort handler of task {} ({}) was cut off {restarts} times", record.id, record.kind)
                } else {
                    format!("Task {} ({}) was cut off {restarts} times without progress", record.id, record.kind)
                };
                let error = TaskOutcomeError {
                    message: message.clone(),
                    detail: Some(Json::from(serde_json::json!({ "reason": "restart_limit" }))),
                };
                self.terminate(tx, &record, TaskOutcome::Faulted { error }).await?;
                pass.faulted.push((record, message));
                continue;
            }
            let snapshot = snapshot.get_or_insert_with(|| self.core.registry.snapshot()).clone();
            let (task, resolved) = match self.resolve(&record, &snapshot) {
                Ok(resolved) => resolved,
                Err(reason) => {
                    // An abandoned task waits for its definition, so its abort handler can clean up.
                    if mode == Mode::Abort && record.abort_reason.is_none() {
                        self.terminate(tx, &record, TaskOutcome::Orphaned { reason: reason.as_str().into() }).await?;
                    }
                    continue;
                }
            };
            if resolved != record || record.status() != TaskStatus::Running {
                let checkpoint = resolved.state.checkpoint().cloned().unwrap_or(Json::Null);
                tx.set_task(with_state(&resolved, TaskState::Running { checkpoint }))?;
            }
            let invocation = self.create_invocation(&record, mode, installer_of(&snapshot, Some(&task)));
            // Registered on the line, so marks and later reservations see it and close joins it; `end` makes it a
            // candidate again.
            lock(&self.state).runnable.remove(&id);
            reservations.push(Reservation { invocation, task, snapshot });
        }
        Ok(pass)
    }

    /// Resolve the record's definition by kind, migrating an older stored version.
    pub(crate) fn resolve(
        &self,
        record: &TaskRecord,
        snapshot: &RegistrySnapshot,
    ) -> Result<(Task, TaskRecord), BlockedReason> {
        let fit = self.fit(record.id, record.version, snapshot.task(&record.kind));
        let (task, migrates) = fit.map_err(|(reason, _)| reason)?;
        if !migrates {
            return Ok((task, record.clone()));
        }
        let checkpoint = record.state.checkpoint().cloned().unwrap_or(Json::Null);
        match task.run_migrate(&record.input, &checkpoint, record.version) {
            Ok((input, checkpoint)) => {
                let state = match &record.state {
                    TaskState::Waiting { on, policy, .. } => {
                        TaskState::Waiting { checkpoint, on: on.clone(), policy: *policy }
                    }
                    TaskState::Sleeping { until, .. } => TaskState::Sleeping { checkpoint, until: *until },
                    TaskState::Running { .. } => TaskState::Running { checkpoint },
                    _ => TaskState::Pending { checkpoint },
                };
                Ok((task.clone(), TaskRecord { version: task.version, input, state, ..record.clone() }))
            }
            Err(error) => {
                let extension = installer_of(snapshot, Some(&task));
                lock(&self.state).failed_migrations.insert(record.id, (task, error.clone()));
                // Blocked now without a commit: the graph and the host's wake must hear it.
                self.notify_blocked();
                self.schedule_wake();
                let info = ReportInfo::MigrationFailed { task: report_task(record), extension };
                self.core.report(&error, &info, None);
                Err(BlockedReason::MigrationFailed)
            }
        }
    }

    #[allow(clippy::type_complexity)]
    pub(super) fn fit(
        &self,
        id: TaskId,
        version: u32,
        task: Option<Task>,
    ) -> Result<(Task, bool), (BlockedReason, Option<DurableError>)> {
        let Some(task) = task else { return Err((BlockedReason::MissingTask, None)) };
        if task.version == version {
            return Ok((task, false));
        }
        if task.version < version {
            return Err((BlockedReason::TaskTooOld, None));
        }
        if let Some((failed, error)) = lock(&self.state).failed_migrations.get(&id)
            && Arc::ptr_eq(failed, &task)
        {
            return Err((BlockedReason::MigrationFailed, Some(error.clone())));
        }
        Ok((task, true))
    }

    /// Scheduling state and every live task with its derived state. Runs no task code.
    pub(crate) async fn inspect(
        &self,
        snapshot: &RegistrySnapshot,
    ) -> DurableResult<(Scheduling, Vec<TaskInspection>)> {
        self.load_scopes(false).await?;
        let records: Vec<TaskRecord> = lock(&self.state).live.values().cloned().collect();
        let tasks = records
            .into_iter()
            .map(|record| TaskInspection { state: self.inspect_task(&record, snapshot), record })
            .collect();
        let state = lock(&self.state);
        let scheduling = if state.closing {
            Scheduling::Closing
        } else if state.enabled {
            Scheduling::Running
        } else {
            Scheduling::Paused
        };
        Ok((scheduling, tasks))
    }

    fn inspect_task(&self, record: &TaskRecord, snapshot: &RegistrySnapshot) -> TaskInspectionState {
        {
            let state = lock(&self.state);
            if state.invocations.contains_key(&record.id) {
                return TaskInspectionState::Running;
            }
            if record.status() == TaskStatus::Completing {
                return TaskInspectionState::Completing;
            }
            let on = waiting_on(&state, record, false);
            if !on.is_empty() {
                return TaskInspectionState::Waiting { on };
            }
            if let TaskState::Sleeping { until, .. } = record.state
                && not_due(record, self.core.now())
            {
                return TaskInspectionState::Sleeping { until };
            }
        }
        match self.fit(record.id, record.version, snapshot.task(&record.kind)) {
            Err((reason, error)) => TaskInspectionState::Blocked { reason, error: error.map(|e| e.to_string()) },
            Ok((task, true)) if !task.has_migrate() => TaskInspectionState::Blocked {
                reason: BlockedReason::MigrationFailed,
                error: Some(missing_migration(&record.kind, task.version, record.version).to_string()),
            },
            Ok((_, migrates)) => TaskInspectionState::Ready { migrates },
        }
    }

    fn create_invocation(&self, record: &TaskRecord, mode: Mode, extension: Option<String>) -> Arc<Invocation> {
        let controller = AbortController::new();
        let context = with_abort_signal(&controller.signal(), &self.context);
        let (done, _) = watch::channel(false);
        let invocation = Arc::new(Invocation {
            task_id: record.id,
            kind: record.kind.clone(),
            extension,
            conversation_id: record.conversation_id,
            mode,
            controller,
            context: Mutex::new(context),
            links: if record.started_at.is_none() { record.links.clone().unwrap_or_default() } else { Vec::new() },
            record: Mutex::new(record.clone()),
            watches: Mutex::new(Vec::new()),
            timers: Mutex::default(),
            next_timer: Default::default(),
            ended: Default::default(),
            done,
        });
        lock(&self.state).invocations.insert(record.id, invocation.clone());
        invocation
    }
}

/// A sleep whose `until` the clock has not reached; an abort mark ends a sleep.
pub(super) fn not_due(record: &TaskRecord, now: f64) -> bool {
    matches!(record.state, TaskState::Sleeping { until, .. } if !record.abort_requested && until > now)
}

/// Live tasks a task waits for: its live owned work when abort-marked, else the live part of a wait. With `first`,
/// at most one, which is all a reservation pass asks.
fn waiting_on(state: &SchedState, record: &TaskRecord, first: bool) -> Vec<TaskId> {
    if record.abort_requested {
        let mut owned = state.owned_live(record.id, None, first, None);
        owned.sort_unstable();
        return owned;
    }
    let TaskState::Waiting { on, .. } = &record.state else { return Vec::new() };
    let mut live = on.iter().copied().filter(|id| state.live.contains_key(id));
    if first { live.next().into_iter().collect() } else { live.collect() }
}
