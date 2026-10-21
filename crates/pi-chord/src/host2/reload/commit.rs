//! The commit: installs the prepared code version in one synchronous step. Observers (state, shapes, registries,
//! listener roots) hear about it after the whole step; work that starts new-code-version roots runs last.

use std::collections::HashSet;
use std::sync::Arc;

use super::{Candidate, Reload, ReloadReport, Start, key, reload_error};
use crate::error::{ChordError, ChordResult};
use crate::host2::contract::{Contract, bind};
use crate::host2::errors::{ChordErrorCode, ErrorOptions};
use crate::host2::exec::spawn_now;
use crate::host2::host::id;
use crate::host2::object::{HostCore, Installed, ObjectImpl, ObjectStatus, Ref, RefCore, RefOwner};
use crate::host2::publish::hold_publications;

impl Reload {
    /// Publishes the new code version. Everything fallible ran in prepare, so this only installs. If an install
    /// still fails, the code version is half-published: the host fails for good and admits no more work.
    pub(crate) fn commit(&self, before_start: Option<Start>) -> ChordResult<ReloadReport> {
        let result = hold_publications(|| {
            let report = self.publish()?;
            // Installed, not started yet: the coordinator queues its commit frames here, so nothing this code
            // version starts can reach a child first.
            if let Some(before_start) = before_start {
                before_start();
            }
            let starts = std::mem::take(&mut self.inner().starts);
            for start in starts {
                start();
            }
            Ok(report)
        });
        result.map_err(|error: ChordError| {
            self.host.fail(error.clone());
            reload_error(error)
        })
    }

    fn publish(&self) -> ChordResult<ReloadReport> {
        let host = &self.host;
        let (older, next, objects) = {
            let mut state = host.state();
            let Some(next) = state.pending.take() else { return Err(ChordError::error("no reload to commit")) };
            let (older, id) = (state.current, next.id);
            state.code_versions.insert(id, next);
            state.previous = Some(older);
            state.current = id;
            (older, id, state.objects.values().cloned().collect::<Vec<_>>())
        };
        let mut inner = self.inner();
        let mut candidates = std::mem::take(&mut inner.candidates);
        let removed = inner.removed.clone();
        let mut starts: Vec<Start> = Vec::new();
        let mut prepared = Vec::new();
        for object in &objects {
            if removed.contains(object.type_name()) {
                continue;
            }
            let Some(candidate) = candidates.get_mut(&key(object)) else {
                let mut state = object.state();
                if let Some(installed) = state.implementations.get(&older).cloned() {
                    state.implementations.insert(next, installed);
                }
                continue;
            };
            if object.status() != ObjectStatus::Live {
                continue;
            }
            self.install(candidate, next, &mut starts)?;
            prepared.push(object.address().to_string());
        }
        for candidate in candidates.values_mut() {
            if candidate.object.status() == ObjectStatus::Live {
                continue;
            }
            if let Some(staged) = candidate.staged.take() {
                staged.discard();
            }
            let (object, run) = (candidate.object.clone(), candidate.run.clone());
            object.state().pending.retain(|decls| !Arc::ptr_eq(decls, &run.decls));
            spawn_now(host.handle(), async move {
                if let Err(error) = object.unwind(&run).await {
                    object.host().report(error);
                }
            });
        }
        // Objects of types this reload adds start their runtime now.
        for (object, staged) in std::mem::take(&mut inner.deferred) {
            if object.status() != ObjectStatus::Live {
                staged.discard();
                continue;
            }
            object.state().deferred_runtime = false;
            staged.activate();
            starts.push(Box::new(move || object.start_tasks()));
        }
        host.install_tables(next, std::mem::take(&mut inner.tables), true);
        let reason = ChordError::code_only(ChordErrorCode::Gone, "type no longer served");
        for object in objects.iter().filter(|object| removed.contains(object.type_name())) {
            let (object, reason) = (object.clone(), reason.clone());
            spawn_now(host.handle(), async move {
                if let Err(error) = object.dispose(true, Some(reason)).await {
                    object.host().report(error);
                }
            });
        }
        {
            let mut state = host.state();
            for name in &removed {
                state.host_objects.shift_remove(name);
            }
            for facet in &self.facets {
                state.facets.insert(facet.name().to_string(), facet.clone());
            }
            for name in &self.options.remove {
                state.facets.shift_remove(name);
            }
            state.gating = None;
        }
        // The replaced and removed runs' signals abort after the install, like other observers.
        for (name, run) in inner.facet_runs.clone() {
            let host = host.clone();
            starts.push(Box::new(move || host.facet_run_changed(&name, Some(run))));
        }
        for name in self.options.remove.clone() {
            let host = host.clone();
            starts.push(Box::new(move || host.facet_run_changed(&name, None)));
        }
        for (name, service) in inner.services.clone() {
            let pinned = host.state().host_objects.get(&name).cloned();
            let live = pinned.as_ref().and_then(|pinned| pinned.object()).map(|object| object.status());
            if service.host_lifetime() && inner.changed.contains(&name) && live != Some(ObjectStatus::Live) {
                let (host, service) = (host.clone(), service.clone());
                starts.push(Box::new(move || {
                    if let Err(error) = host.start_host_object(&service) {
                        host.report(error);
                    }
                }));
            }
            // A type whose lifetime became "referenced" lives on only while someone else references its object.
            if let (false, Some(pinned)) = (service.host_lifetime(), pinned) {
                host.state().host_objects.shift_remove(&name);
                starts.push(Box::new(move || pinned.release()));
            }
        }
        let gated = inner.gated.iter().filter_map(|key| candidates.get(key));
        let report = ReloadReport {
            code_version: next,
            previous: older,
            changed: inner.changed.iter().cloned().collect(),
            removed: removed.iter().cloned().collect(),
            prepared,
            gated: gated.map(|candidate| candidate.object.address().to_string()).collect(),
            children: Default::default(),
        };
        inner.starts.extend(starts);
        Ok(report)
    }

    fn install(&self, candidate: &mut Candidate, code_version: i64, starts: &mut Vec<Start>) -> ChordResult<()> {
        let (object, run, service) = (candidate.object.clone(), candidate.run.clone(), candidate.service.clone());
        let gated = !candidate.reasons.is_empty();
        object.state().pending.retain(|decls| !Arc::ptr_eq(decls, &run.decls));
        object.stop_tasks();
        if gated {
            // Old derivations would compute over migrated values; the old implementation can no longer run anyway.
            let kept: HashSet<usize> = run.decls.data().derives.values().map(id).collect();
            let old: Vec<_> = object.decls().data().derives.values().cloned().collect();
            old.iter().filter(|derived| !kept.contains(&id(derived))).for_each(|derived| derived.detach());
        }
        {
            let mut state = object.state();
            state.decls = run.decls.clone();
            state.service = Some(service.clone());
        }
        // Kept resources close with the new code's `close`; replaced ones keep their own (closed at retirement).
        let adopted = std::mem::take(&mut run.decls.data().adopted);
        for (resource, declaration) in adopted {
            resource.adopt(declaration);
        }
        let installed = Installed {
            service: service.clone(),
            implementation: run.implementation.clone(),
            decls: run.decls.clone(),
        };
        object.state().implementations.insert(code_version, installed);
        if gated {
            // Old code can never run over migrated state: the older implementations go now, before retirement.
            let olds: Vec<i64> = {
                let mut state = object.state();
                state.min_code_version = code_version;
                state.implementations.keys().copied().filter(|old| *old != code_version).collect()
            };
            for old in olds {
                self.host.retire_implementation(&object, old);
            }
        }
        // Schemas first, then the values validated against them in prepare.
        object.refresh_schemas();
        let cells: Vec<_> = {
            let data = run.decls.data();
            data.cells.iter().map(|(name, cell)| (cell.clone(), data.cell_schemas.get(name).cloned())).collect()
        };
        for (cell, schema) in cells {
            let schema = schema.unwrap_or_default();
            match candidate.migrated.iter().find(|(migrated, _)| Arc::ptr_eq(migrated, &cell)) {
                Some((_, value)) => cell.reset(value.clone(), schema)?,
                None => cell.set_schema(schema),
            }
        }
        let created: HashSet<usize> = run.created.derives.iter().map(id).collect();
        for derived in &run.created.derives {
            derived.attach(Some(&candidate.derived))?;
        }
        let kept: Vec<_> = run.decls.data().derives.values().filter(|d| !created.contains(&id(d))).cloned().collect();
        kept.iter().for_each(|derived| derived.refresh());
        if let Some(staged) = candidate.staged.take() {
            staged.activate();
        }
        let started = object.clone();
        starts.push(Box::new(move || started.start_tasks()));
        object.open_gate();
        self.follow_host_object(&object, &service.contract);
        for reference in object.refs() {
            // Browser references whose contract no longer binds are told to reload their code (they never block).
            if reference.owner() == RefOwner::Client
                && let Err(error) = bind(&service.contract, service.options.accepts.as_ref(), reference.contract())
            {
                let message = format!("{} changed; reload the web facets", object.type_name());
                let options = ErrorOptions::default().with_cause(error);
                reference.gone(&ChordError::coded(ChordErrorCode::UpgradeRequired, message, options));
                object.release_ref(&reference);
                continue;
            }
            reference.shape_changed();
        }
        Ok(())
    }

    /// The host's own reference to a host object follows a contract change, without reopening the object.
    fn follow_host_object(&self, object: &Arc<ObjectImpl>, contract: &Contract) {
        let pinned = self.host.state().host_objects.get(object.type_name()).cloned();
        let Some(pinned) = pinned else { return };
        let same = pinned.object().is_some_and(|pinned| Arc::ptr_eq(&pinned, object));
        if !same || pinned.contract().fingerprint() == contract.fingerprint() {
            return;
        }
        // Get the new reference first: releasing the only reference must not dispose and reopen the object.
        let Some(replacement) = object.new_ref(contract.clone(), RefOwner::Host) else { return };
        self.host.state().host_objects.insert(object.type_name().to_string(), Ref::new(replacement));
        pinned.release();
    }
}
