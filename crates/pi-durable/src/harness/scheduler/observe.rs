//! The scheduler's commit and close listeners.

use super::{Mode, Node, Scheduler, cancellation_intent, failed_outcome};
use crate::harness::util::{closed_error, lock};
use crate::session::ObservedCommit;
use crate::types::{CommitChange, JoinPolicy, SubmissionStatus, SubmissionType, TaskRecord, TaskState, TaskStatus};

impl Scheduler {
    /// The commit listener: keeps `live` in step with committed task records.
    pub(super) fn observe(&self, publication: &ObservedCommit) {
        // A commit held in memory changes documents only.
        if publication.is_memory() {
            return;
        }
        let mut resolved = Vec::new();
        let mut signal = Vec::new();
        let mut reconcile = false;
        let mut changed = false;
        {
            let mut state = lock(&self.state);
            let mut updated = Vec::new();
            let mut failed = Vec::new();
            // Edges first, so the tasks a commit creates in a new conversation hang from it.
            for change in publication.changes() {
                if let CommitChange::Conversation(record) = change
                    && !state.edges.contains_key(&record.id)
                {
                    state.set_edge(record.id, record.owner.map(|owner| owner.task_id));
                }
            }
            for change in publication.changes() {
                let CommitChange::Task(record) = change else { continue };
                changed = true;
                let previous = state.live.get(&record.id).cloned();
                if let Some(invocation) = state.invocations.get(&record.id) {
                    *lock(&invocation.record) = record.clone();
                }
                if record.is_terminal() {
                    state.resumed.remove(&record.id);
                }
                if !state.closing && record.is_terminal() != previous.is_none() {
                    self.count_active(&record.kind, if record.is_terminal() { -1.0 } else { 1.0 });
                }
                if failed_outcome(record) && previous.as_ref().is_none_or(|p| !failed_outcome(p)) {
                    failed.push(record.id);
                }
                state.track(previous.as_ref(), record);
                if record.is_terminal() {
                    state.failed_migrations.remove(&record.id);
                    state.fail_fast_checks.remove(&record.id);
                    resolved.push(record.clone());
                    // Its owner may finalize now.
                    reconcile = true;
                    continue;
                }
                let was_requested = previous.as_ref().is_some_and(|p| p.abort_requested);
                if record.abort_requested && !was_requested {
                    state.cascade_pending = true;
                    if let Some(invocation) = state.invocations.get(&record.id)
                        && invocation.mode == Mode::Run
                    {
                        signal.push(invocation.clone());
                    }
                }
                // A request replacing a `restart` mark upgrades the marks of the work below.
                if previous.as_ref().is_some_and(|p| p.abort_reason.is_some()) && record.abort_reason.is_none() {
                    state.cascade_pending = true;
                }
                let previous_status = previous.as_ref().map(TaskRecord::status);
                if record.status() == TaskStatus::Completing && previous_status != Some(TaskStatus::Completing) {
                    if cancellation_intent(record) {
                        state.cascade_pending = true;
                    }
                    reconcile = true;
                }
                if let TaskState::Waiting { policy: JoinPolicy::FailFast, .. } = record.state
                    && previous_status != Some(TaskStatus::Waiting)
                {
                    state.fail_fast_checks.insert(record.id);
                    reconcile = true;
                }
                // Its owners and place never change: what is above it is checked once, when it becomes live.
                if previous.is_none() {
                    updated.push(record.clone());
                }
            }
            for id in failed {
                let waiters: Vec<_> = state.waiters.get(&id).into_iter().flatten().copied().collect();
                for waiter in waiters {
                    let waiting = state.live.get(&waiter).map(|record| &record.state);
                    if matches!(waiting, Some(TaskState::Waiting { policy: JoinPolicy::FailFast, .. })) {
                        state.fail_fast_checks.insert(waiter);
                        reconcile = true;
                    }
                }
            }
            for change in publication.changes() {
                // A queued input below a cancelled owner is withdrawn, even after its cascade.
                let CommitChange::Submission(record) = change else { continue };
                if record.status != SubmissionStatus::Queued || record.kind != SubmissionType::Input {
                    continue;
                }
                // Only an owner with cancellation intent withdraws it; a later intent's mark runs the cascade then.
                if state.intent.is_empty() {
                    continue;
                }
                let conversation = Node::Conversation(record.conversation_id);
                if !state.known(conversation) || state.below_cancelled(conversation) {
                    state.cascade_pending = true;
                }
            }
            for record in &updated {
                // Work created below a cancelled owner, even after its cascade, is aborted too.
                if state.unloaded.contains(&record.id) {
                    reconcile = true;
                } else if !record.background && !record.abort_requested && state.below_cancelled(Node::of(record)) {
                    state.cascade_pending = true;
                }
            }
            if state.cascade_pending {
                reconcile = true;
            }
            state.sweep();
        }
        for record in resolved {
            let id = record.id;
            self.task_waiters.resolve(&id, record);
        }
        // Abort listeners are task code, which never runs inside Storage's `adopt`: the signal follows in a task, after
        // host commit listeners.
        if !signal.is_empty() {
            tokio::spawn(async move {
                for invocation in signal {
                    invocation.controller.abort(None);
                }
            });
        }
        if reconcile {
            self.schedule_reconcile();
        }
        if !changed {
            return;
        }
        self.settle_idle();
        self.kick();
    }

    /// Close listener: runs once admission is sealed, before `join()`, or when the Session fails.
    pub(super) fn seal(&self) {
        let (unsubscribe, timers) = {
            let mut state = lock(&self.state);
            if !state.closing {
                for record in state.live.values() {
                    self.count_active(&record.kind, -1.0);
                }
            }
            state.resumed.clear();
            state.closing = true;
            state.contexts.clear();
            let timers = [state.expiry.take(), state.timer.take()];
            (state.unsubscribe_registry.take(), timers)
        };
        for (_, disarm) in timers.into_iter().flatten() {
            disarm();
        }
        if let Some(unsubscribe) = unsubscribe {
            unsubscribe();
        }
        let error = closed_error(&self.core.session);
        self.task_waiters.reject_all(error.clone());
        self.idle_waiters.reject_all(error);
    }
}
