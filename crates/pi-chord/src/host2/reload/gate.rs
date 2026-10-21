//! The gate (admission cut, grandfathered roots, exclusive resources, migration), the checks of every prospective
//! value, the retirement a reload waits for first, and the rollback.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::join_all;
use tokio::sync::oneshot;

use super::Reload;
use crate::context::AbortController;
use crate::error::{ChordError, ChordResult};
use crate::host2::errors::{ChordErrorCode, ErrorOptions};
use crate::host2::object::{Gate, HostCore, ObjectImpl, member_schemas};
use crate::host2::schema::{assert_valid, deep_equal};
use crate::host2::types::{Readable, ReadableRef};
use crate::json::Json;
use crate::sync::{BoxFuture, SharedTask, lock};

/// The most a forced retirement waits for aborted roots to settle before it closes what they used.
const ABORTED_ROOTS_GRACE: Duration = Duration::from_secs(1);

/// A source's value for a dry run. A reference to a provider this reload upgrades too reads nothing before the
/// commit (its new contract does not bind the old code): the dry run computes without it, the commit with it.
fn candidate_value(source: &ReadableRef) -> ChordResult<Json> {
    match source.value() {
        Err(error) if matches!(error.code(), Some(ChordErrorCode::NotReady | ChordErrorCode::NotInVariant)) => {
            Ok(Json::Null)
        }
        other => other,
    }
}

impl Reload {
    /// Retires the older code version before loading the next one (a host keeps two). Waits until its roots ended
    /// and, on a child host, its parent retired it too, at most the retire deadline. Past it, the remaining roots
    /// are aborted and get a short grace to stop before the resources only that code version used are closed.
    pub(super) async fn wait_for_retirement(&self) {
        let host = &self.host;
        let Some(older) = host.previous_id() else { return };
        let deadline = self.options.retire_deadline();
        let gone = || host.previous_id() != Some(older);
        if !host.retired_until(|| gone() || host.retirable(), deadline).await && !gone() {
            let abort = host.state().code_versions.get(&older).map(|version| version.abort.clone());
            let message = format!("code version {older} was retired");
            if let Some(abort) = abort {
                abort.abort(Some(ChordError::code_only(ChordErrorCode::Superseded, message)));
            }
            let grace = deadline.min(ABORTED_ROOTS_GRACE);
            host.retired_until(|| gone() || host.roots_of(older) == 0, grace).await;
        }
        if !gone() {
            host.retire();
        }
    }

    /// Admission cut, wait for grandfathered roots (deadline), replace exclusive resources, migrate into private
    /// copies. Old tasks of gated objects stop at the cut; listener events queue until commit or abort.
    pub(super) async fn gate(&self) -> ChordResult<()> {
        let gated: Vec<(usize, Arc<ObjectImpl>)> = {
            let mut inner = self.inner();
            let gated: Vec<(usize, Arc<ObjectImpl>)> = inner
                .candidates
                .iter()
                .filter(|(_, c)| !c.reasons.is_empty() && c.object.status() == crate::host2::object::ObjectStatus::Live)
                .map(|(key, c)| (*key, c.object.clone()))
                .collect();
            inner.gated = gated.iter().map(|(key, _)| *key).collect();
            gated
        };
        let mut drained = Vec::new();
        for (_, object) in &gated {
            let (done, wait) = oneshot::channel::<()>();
            let empty = {
                let mut state = object.state();
                let grandfathered: HashSet<String> = state.leases.keys().cloned().collect();
                let empty = grandfathered.is_empty();
                let drain: Box<dyn FnOnce() + Send> = Box::new(move || {
                    let _ = done.send(());
                });
                state.gate = Some(Gate { grandfathered, queue: Vec::new(), drained: Some(drain) });
                empty
            };
            object.stop_tasks();
            if !empty {
                drained.push(wait);
            }
        }
        let deadline = self.options.gate_deadline();
        if tokio::time::timeout(deadline, join_all(drained)).await.is_err() {
            let mut blocking: Vec<String> = Vec::new();
            for (_, object) in &gated {
                let mut roots: Vec<String> = object.state().gate.iter().flat_map(|g| g.grandfathered.clone()).collect();
                roots.sort();
                blocking.extend(roots);
            }
            let message =
                format!("gate deadline ({} ms) missed; blocking roots: {}", deadline.as_millis(), blocking.join(", "));
            let details = crate::json!({ "blocking": blocking });
            let options = ErrorOptions::reason("gate_deadline").with_details(details);
            return Err(ChordError::coded(ChordErrorCode::Aborted, message, options));
        }
        self.alive()?;
        let signal = self.abort.signal();
        for (key, object) in &gated {
            let Some((replaced, at_gate, run)) =
                self.inner().candidates.get(key).map(|c| (c.replaced.clone(), c.at_gate.clone(), c.run.clone()))
            else {
                continue;
            };
            for old in replaced {
                // Owned by the rollback before closing: a failing close must still be restored.
                self.inner().closed.push((object.clone(), old.clone()));
                old.close(&object.open_order).await?;
            }
            object.open(&at_gate, &[], &signal).await?;
            self.alive()?;
            object.init_cells(&run, &signal).await?;
            self.alive()?;
        }
        for (key, _) in &gated {
            self.migrate(*key)?;
        }
        Ok(())
    }

    /// Migrates the kept cells whose version rose (or whose schema changed) into private values, validated.
    fn migrate(&self, key: usize) -> ChordResult<()> {
        let (object, run) = match self.inner().candidates.get(&key) {
            Some(candidate) => (candidate.object.clone(), candidate.run.clone()),
            None => return Ok(()),
        };
        let earlier = object.decls();
        let mut migrated = Vec::new();
        {
            let (old, new) = (earlier.data(), run.decls.data());
            for (name, cell) in &new.cells {
                if !old.cells.get(name).is_some_and(|kept| Arc::ptr_eq(kept, cell)) {
                    continue;
                }
                let schema = new.cell_schemas.get(name).cloned().unwrap_or_default();
                let from = old.cell_versions.get(name).copied().unwrap_or(1);
                let raised = new.cell_versions.get(name).copied().unwrap_or(1) > from;
                let value = match new.cell_migrations.get(name) {
                    Some(migrate) if raised => migrate(cell.value()?, from)?,
                    _ if !deep_equal(&cell.schema(), &schema) => cell.value()?,
                    _ => continue,
                };
                let what = format!("{}: migrated state {name}", object.address());
                assert_valid("state", &what, &schema, Some(&value))?;
                migrated.push((cell.clone(), value));
            }
        }
        if let Some(candidate) = self.inner().candidates.get_mut(&key) {
            candidate.migrated = migrated;
        }
        self.dry_run(key)
    }

    /// Computes the new derivations over the candidate values and validates every prospective value (migrated cells
    /// and derivations) against every schema that applies after the commit. The commit installs without checks.
    pub(super) fn dry_run(&self, key: usize) -> ChordResult<()> {
        let Some((object, service, run, migrated)) = self
            .inner()
            .candidates
            .get(&key)
            .map(|c| (c.object.clone(), c.service.clone(), c.run.clone(), c.migrated.clone()))
        else {
            return Ok(());
        };
        let exposed = member_schemas(&service, &run.implementation);
        let mut values: HashMap<usize, Json> = HashMap::new();
        for (cell, value) in &migrated {
            for schema in exposed.get(&cell.core().id()).into_iter().flatten() {
                let what = format!("{}: migrated state {}", object.address(), cell.key());
                assert_valid("state", &what, schema, Some(value))?;
            }
            values.insert(cell.core().id(), value.clone());
        }
        let derives: Vec<(String, Arc<crate::host2::cell::Derived>)> =
            run.decls.data().derives.iter().map(|(name, derived)| (name.clone(), derived.clone())).collect();
        for (name, derived) in derives {
            let read = |source: &ReadableRef| match source.source().and_then(|core| values.get(&core.id())) {
                Some(value) => Ok(value.clone()),
                None => candidate_value(source),
            };
            let value = derived.compute(read)?;
            for schema in exposed.get(&derived.core().id()).into_iter().flatten() {
                let what = format!("{}: derivation {name}", object.address());
                assert_valid("state", &what, schema, Some(&value))?;
            }
            values.insert(derived.core().id(), value);
        }
        if let Some(candidate) = self.inner().candidates.get_mut(&key) {
            candidate.derived = values;
        }
        Ok(())
    }

    /// Undoes everything prepare did. Once: a second call waits for the first.
    pub(crate) fn rollback(self: &Arc<Self>) -> BoxFuture<()> {
        let task = {
            let mut slot = lock(&self.rollback);
            match &*slot {
                Some(task) => task.clone(),
                None => {
                    let me = self.clone();
                    let task: BoxFuture<ChordResult<()>> = Box::pin(async move {
                        me.roll_back().await;
                        Ok(())
                    });
                    let task: SharedTask = task.shared();
                    *slot = Some(task.clone());
                    task
                }
            }
        };
        Box::pin(async move {
            let _ = task.await;
        })
    }

    async fn roll_back(&self) {
        let host = &self.host;
        let (candidates, deferred, closed, gated, facet_runs) = {
            let mut inner = self.inner();
            inner.rolled_back = true;
            let candidates: Vec<_> =
                inner.candidates.values_mut().map(|c| (c.object.clone(), c.run.clone(), c.staged.take())).collect();
            let gated: Vec<Arc<ObjectImpl>> =
                inner.gated.iter().filter_map(|key| inner.candidates.get(key)).map(|c| c.object.clone()).collect();
            let facet_runs: Vec<AbortController> = inner.facet_runs.values().cloned().collect();
            (candidates, std::mem::take(&mut inner.deferred), inner.closed.clone(), gated, facet_runs)
        };
        let aborted = || ChordError::code_only(ChordErrorCode::Aborted, "reload aborted");
        self.abort.abort(Some(aborted()));
        for run in facet_runs {
            run.abort(Some(aborted()));
        }
        let mut objects = Vec::new();
        for (object, run, staged) in candidates {
            if let Some(staged) = staged {
                staged.discard();
            }
            objects.push((object, run));
        }
        for (_, staged) in deferred {
            staged.discard();
        }
        for (object, run) in objects {
            object.state().pending.retain(|decls| !Arc::ptr_eq(decls, &run.decls));
            if let Err(error) = object.unwind(&run).await {
                host.report(error);
            }
        }
        // Replaced exclusive resources are restored; one that cannot be is marked unavailable, never closed silently.
        for (object, resource) in closed {
            if !resource.is_closed() {
                continue;
            }
            if let Err(error) = resource.open(&AbortController::new().signal(), &object.open_order).await {
                resource.mark_unavailable();
                let message = format!("{}: resource {} could not be restored", object.address(), resource.key());
                let options = ErrorOptions::default().with_cause(error);
                host.report(ChordError::coded(ChordErrorCode::Unavailable, message, options));
            }
        }
        for object in gated {
            if object.state().gate.is_none() {
                continue;
            }
            object.open_gate();
            object.start_tasks();
        }
        // Objects of types this reload would have added lose their code.
        let reason = ChordError::code_only(ChordErrorCode::Gone, "reload aborted; type not served");
        let objects: Vec<Arc<ObjectImpl>> = host.state().objects.values().cloned().collect();
        for object in objects {
            let served = {
                let state = host.state();
                state.code_versions.get(&state.current).is_some_and(|v| v.services.contains_key(object.type_name()))
            };
            if !served && let Err(error) = object.dispose(true, Some(reason.clone())).await {
                host.report(error);
            }
        }
        let mut state = host.state();
        state.pending = None;
        state.gating = None;
    }
}
