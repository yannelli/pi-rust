//! Reconciliation: cascade marks, `failFast` marks, withdrawn inputs, and final records of held outcomes. Also
//! idle settlement, the kept-context expiry, and the reservation drain loop.

use std::collections::HashMap;

use super::ownership::Overlay;
use super::{Scheduler, Up, failed_outcome, with_abort_mark, with_state};
use crate::errors::DurableResult;
use crate::harness::live::settle_scheduler_outcome;
use crate::harness::util::lock;
use crate::ids::TaskId;
use crate::session::{TransactionScope, Tx};
use crate::timers::Disarm;
use crate::types::{AbortReason, HostCallback, ReportInfo, TaskOutcome, TaskRecord, TaskState, TaskStatus};

impl Scheduler {
    pub(crate) fn schedule_reconcile(&self) {
        {
            let mut state = lock(&self.state);
            if state.reconcile_scheduled || state.closing {
                return;
            }
            state.reconcile_scheduled = true;
        }
        let Ok(scheduler) = self.arc() else { return };
        tokio::spawn(async move { scheduler.reconcile().await });
    }

    /// One commit that applies what committed records imply: cascade and `failFast` marks, withdrawn inputs
    /// below cancelled owners, and the final record of every `completing` task without live owned work.
    async fn reconcile(&self) {
        let (cascade, checks, work) = {
            let mut state = lock(&self.state);
            let work = state.reconcile_work();
            state.reconciling += usize::from(work);
            state.reconcile_scheduled = false;
            let cascade = std::mem::take(&mut state.cascade_pending);
            (cascade, std::mem::take(&mut state.fail_fast_checks).into_iter().collect::<Vec<_>>(), work)
        };
        let Ok(scheduler) = self.arc() else { return };
        let result = self
            .core
            .session
            .commit_with(
                move |tx| async move { scheduler.reconcile_in(&tx, cascade, checks).await },
                &self.context,
                TransactionScope::default(),
            )
            .await;
        if let Err(error) = result {
            // No extension code runs in this commit: a failure is a storage failure, a host callback, or a bug.
            self.fail_session(error);
        }
        lock(&self.state).reconciling -= usize::from(work);
        self.schedule_wake();
        self.settle_idle();
    }

    async fn reconcile_in(&self, tx: &Tx, cascade: bool, checks: Vec<TaskId>) -> DurableResult<()> {
        if self.closing() {
            return Ok(());
        }
        let queued = self.load_scopes(cascade).await?;
        let mut marks: HashMap<TaskId, (TaskRecord, Option<AbortReason>)> = HashMap::new();
        let mut order: Vec<TaskId> = Vec::new();
        let mut mark = |record: &TaskRecord, reason: Option<AbortReason>| {
            if let Some((_, staged)) = marks.get(&record.id)
                && staged.is_none()
            {
                return;
            }
            if record.abort_requested && (record.abort_reason.is_none() || reason.is_some()) {
                return;
            }
            if !marks.contains_key(&record.id) {
                order.push(record.id);
            }
            marks.insert(record.id, (record.clone(), reason));
        };
        let waiting: Vec<(TaskId, Vec<TaskId>)> = {
            let state = lock(&self.state);
            for record in state.live.values() {
                if record.background {
                    continue;
                }
                let Some(owner) = state.cancelling_owner(Up::of(record)) else { continue };
                // Only an owner abandoned after a restart, and nothing else, passes its reason on.
                let restart = owner.abort_reason == Some(AbortReason::Restart) && !failed_outcome(&owner);
                mark(record, restart.then_some(AbortReason::Restart));
            }
            checks
                .iter()
                .filter_map(|id| match state.live.get(id).map(|r| &r.state) {
                    Some(TaskState::Waiting { on, .. }) => Some((*id, on.clone())),
                    _ => None,
                })
                .collect()
        };
        for (_, on) in waiting {
            if !self.any_failed(&on).await? {
                continue;
            }
            let members: Vec<TaskRecord> = {
                let state = lock(&self.state);
                on.iter().filter_map(|member| state.live.get(member).cloned()).collect()
            };
            for record in members.iter().filter(|record| !failed_outcome(record)) {
                mark(record, None);
            }
        }
        for id in order {
            if let Some((record, reason)) = marks.remove(&id) {
                tx.set_task(with_abort_mark(&record, reason))?;
            }
        }
        for id in queued {
            let below = lock(&self.state).below_cancelled(Up::Conversation(id));
            if below {
                crate::harness::inbox::withdraw_queued_inputs(tx, id, "aborted").await?;
            }
        }
        self.finalize(tx).await
    }

    /// Whether any of `ids` holds or ended with an outcome other than `completed`.
    async fn any_failed(&self, ids: &[TaskId]) -> DurableResult<bool> {
        for id in ids {
            let live = lock(&self.state).live.get(id).cloned();
            let record = match live {
                Some(record) => Some(record),
                None => self.core.session.storage().task(*id, &self.context).await?,
            };
            if record.is_some_and(|record| failed_outcome(&record)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Write the terminal record of every `completing` task without live ordinary owned work, repeatedly.
    pub(crate) async fn finalize(&self, tx: &Tx) -> DurableResult<()> {
        loop {
            let overlay = Overlay::of(tx);
            let done: Vec<TaskRecord> = {
                let state = lock(&self.state);
                let owned = state.owned_live(Some(&overlay));
                let records = state.live_records(Some(&overlay));
                records
                    .into_iter()
                    .filter(|r| r.status() == TaskStatus::Completing && !owned.contains_key(&r.id))
                    .collect()
            };
            if done.is_empty() {
                return Ok(());
            }
            for record in done {
                let Some(outcome) = record.state.outcome().cloned() else { continue };
                tx.set_task(with_state(&record, TaskState::Terminal { outcome: outcome.clone() }))?;
                // Only the scheduler writes `faulted` and `orphaned`; their cleanup waits for this commit.
                if matches!(outcome, TaskOutcome::Faulted { .. } | TaskOutcome::Orphaned { .. }) {
                    settle_scheduler_outcome(tx, &record, &outcome).await?;
                }
            }
        }
    }

    /// `context_retention_ms` of the settings.
    fn context_retention_ms(&self) -> f64 {
        self.core.settings().context_retention_ms
    }

    /// Resolve idle waiters, and drop each kept context whose conversation was idle for the retention period.
    pub(crate) fn settle_idle(&self) {
        let retention = self.context_retention_ms();
        let now = self.core.now();
        let idle_keys: Vec<_> = {
            let mut state = lock(&self.state);
            let keys: Vec<_> = self.idle_waiters.keys().into_iter().filter(|key| state.idle(*key)).collect();
            let ids: Vec<_> = state.contexts.keys().copied().collect();
            for id in ids {
                let idle = state.idle(Some(id));
                let Some(kept) = state.contexts.get_mut(&id) else { continue };
                match kept.idle_since {
                    Some(since) if now - since >= retention => {
                        state.contexts.remove(&id);
                    }
                    _ if !idle => kept.idle_since = None,
                    _ if retention > 0.0 => {
                        kept.idle_since.get_or_insert(now);
                    }
                    _ => {
                        state.contexts.remove(&id);
                    }
                }
            }
            keys
        };
        for key in idle_keys {
            self.idle_waiters.resolve(&key, ());
        }
        self.schedule_expiry();
    }

    /// Run `settle_idle()` when the earliest idle context expires, on a background timer. Where the host declines it,
    /// none is kept, and task changes alone check expiry.
    pub(crate) fn schedule_expiry(&self) {
        let retention = self.context_retention_ms();
        let (at, replaced) = {
            let mut state = lock(&self.state);
            let at = state
                .contexts
                .values()
                .filter_map(|kept| kept.idle_since)
                .map(|since| since + retention)
                .reduce(f64::min);
            if at.is_some() && state.expiry.as_ref().map(|(at, _)| *at) == at {
                return;
            }
            (at.filter(|_| !state.closing), state.expiry.take())
        };
        if let Some((_, disarm)) = replaced {
            disarm();
        }
        let Some(at) = at else { return };
        let weak = self.weak.clone();
        let expire = Box::new(move || {
            if let Some(scheduler) = weak.upgrade() {
                lock(&scheduler.state).expiry = None;
                scheduler.settle_idle();
            }
        });
        let Some(disarm) = self.core.timers.background(expire, at - self.core.now()) else { return };
        let stale = lock(&self.state).expiry.replace((at, disarm));
        if let Some((_, disarm)) = stale {
            disarm();
        }
    }

    /// Recompute the wake off the Session line: arm the timer for the earliest sleep, or tell `on_wake` of a change.
    pub(crate) fn schedule_wake(&self) {
        {
            let mut state = lock(&self.state);
            if state.wake_scheduled || state.closing {
                return;
            }
            state.wake_scheduled = true;
        }
        let Ok(scheduler) = self.arc() else { return };
        tokio::spawn(async move { scheduler.deliver_wake() });
    }

    fn deliver_wake(&self) {
        let enabled = {
            let mut state = lock(&self.state);
            state.wake_scheduled = false;
            if state.closing {
                return;
            }
            state.enabled
        };
        let wake = self.wake();
        let Some(on_wake) = &self.core.on_wake else {
            self.arm_timer(if enabled { wake.at } else { None });
            return;
        };
        if lock(&self.state).last_wake == Some(wake) {
            return;
        }
        match on_wake(wake) {
            // Only a delivered wake counts: after an error, the next change or kick offers it again.
            Ok(()) => lock(&self.state).last_wake = Some(wake),
            Err(error) => {
                self.core.report(&error, &ReportInfo::HostError { callback: HostCallback::OnWake, task: None });
            }
        }
    }

    /// Keep the timer for the earliest sleep. The pass it starts rechecks the clock, so a wake before `until` only
    /// arms it again.
    fn arm_timer(&self, at: Option<f64>) {
        let replaced: Option<Disarm> = {
            let mut state = lock(&self.state);
            if state.timer.is_some() && state.timer.as_ref().map(|(at, _)| *at) == at {
                return;
            }
            state.timer.take().map(|(_, disarm)| disarm)
        };
        if let Some(disarm) = replaced {
            disarm();
        }
        let Some(at) = at else { return };
        if self.closing() {
            return;
        }
        let weak = self.weak.clone();
        let fire = Box::new(move || {
            if let Some(scheduler) = weak.upgrade() {
                lock(&scheduler.state).timer = None;
                scheduler.kick();
            }
        });
        let disarm = self.core.timers.foreground(fire, at - self.core.now());
        let stale = lock(&self.state).timer.replace((at, disarm));
        if let Some((_, disarm)) = stale {
            disarm();
        }
    }

    /// Mark the scheduler dirty and start a drain unless one runs.
    pub(crate) fn kick(&self) {
        self.schedule_wake();
        {
            let mut state = lock(&self.state);
            state.dirty = true;
            if state.draining || !state.enabled || state.closing {
                return;
            }
            state.draining = true;
        }
        let Ok(scheduler) = self.arc() else { return };
        tokio::spawn(async move { scheduler.drain().await });
    }

    async fn drain(&self) {
        loop {
            {
                let mut state = lock(&self.state);
                if !(state.dirty && state.enabled && !state.closing) {
                    break;
                }
                state.dirty = false;
            }
            match self.reserve().await {
                Ok(reservations) => {
                    for reservation in reservations {
                        self.start(reservation);
                    }
                }
                Err(error) => {
                    self.fail_session(error);
                    break;
                }
            }
        }
        let dirty = {
            let mut state = lock(&self.state);
            state.draining = false;
            state.dirty
        };
        if dirty {
            self.kick();
        }
    }
}
