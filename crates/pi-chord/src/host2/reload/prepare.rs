//! Prepare: load the facets into a candidate service table, check contracts, rerun the factories of live
//! objects of changed types in recording mode, check the references that survive, and stage listeners.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use futures::future::join_all;
use indexmap::IndexMap;

use super::{Candidate, Reload, key, reload_error};
use crate::context::AbortController;
use crate::error::{ChordError, ChordResult};
use crate::host2::contract::{bind, coverage};
use crate::host2::declare::Dependency;
use crate::host2::errors::{ChordErrorCode, invalid};
use crate::host2::host::{CodeVersion, FacetApi, id};
use crate::host2::object::{HostCore, ObjectImpl, ObjectStatus, RefCore, RefOwner, ResourceImpl, Service};
use crate::host2::registry::Registration;
use crate::host2::schema::deep_equal;
use crate::sync::lock;

/// Whether `object` runs `service` now.
fn runs(object: &ObjectImpl, service: Option<&Arc<Service>>) -> bool {
    let state = object.state();
    matches!((&state.service, service), (Some(own), Some(service)) if Arc::ptr_eq(own, service))
}

impl Reload {
    /// Everything up to the commit. On failure the host is rolled back and the underlying error is returned.
    pub(crate) async fn prepare(self: &Arc<Self>) -> ChordResult<()> {
        if let Err(error) = self.prepare_steps().await {
            self.rollback().await;
            return Err(reload_error(error));
        }
        Ok(())
    }

    async fn prepare_steps(self: &Arc<Self>) -> ChordResult<()> {
        self.wait_for_retirement().await;
        self.load_facets()?;
        self.check_providers()?;
        self.prepare_all().await?;
        // From here new objects of changed types are refused (upgrade_conflict, retryable) until commit or abort,
        // so the candidate set is final once the activations already in flight have been prepared.
        let changed: HashSet<String> = self.inner().changed.iter().cloned().collect();
        self.host.state().gating = Some(changed);
        let deadline = self.options.gate_deadline();
        match tokio::time::timeout(deadline, self.prepare_all()).await {
            Ok(result) => result?,
            Err(_) => {
                let message = format!("activations in flight did not finish within {} ms", deadline.as_millis());
                return Err(ChordError::with_reason(ChordErrorCode::Aborted, "deadline", message));
            }
        }
        self.check_refs()?;
        let tables = self.host.build_tables(self.entries())?;
        self.inner().tables = tables;
        self.gate().await?;
        self.alive()?;
        self.stage()
    }

    /// Builds the candidate service table: unchanged services plus everything the given facets serve.
    fn load_facets(self: &Arc<Self>) -> ChordResult<()> {
        let replaced: HashSet<&str> = self
            .facets
            .iter()
            .map(|facet| facet.name())
            .chain(self.options.remove.iter().map(String::as_str))
            .collect();
        let current: IndexMap<String, Arc<Service>> = {
            let state = self.host.state();
            state.code_versions.get(&state.current).map(|version| version.services.clone()).unwrap_or_default()
        };
        let kept = current.iter().filter(|(_, service)| !replaced.contains(service.facet.as_str()));
        let services = Arc::new(Mutex::new(kept.map(|(name, service)| (name.clone(), service.clone())).collect()));
        for facet in &self.facets {
            let run = AbortController::new();
            self.inner().facet_runs.insert(facet.name().to_string(), run.clone());
            FacetApi::for_reload(&self.host, facet.name(), run.signal(), services.clone()).setup(facet)?;
        }
        let services: IndexMap<String, Arc<Service>> = lock(&services).clone();
        let mut inner = self.inner();
        for (name, service) in &services {
            if current.get(name).is_none_or(|known| !Arc::ptr_eq(known, service)) {
                inner.changed.insert(name.clone());
            }
        }
        for name in current.keys().filter(|name| !services.contains_key(*name)) {
            inner.removed.insert(name.clone());
        }
        let mut pending = CodeVersion::new(self.host.next_code_version_id(self.code_version)?);
        pending.services = services.clone();
        inner.services = services;
        drop(inner);
        self.host.state().pending = Some(pending);
        Ok(())
    }

    /// The same contract version must cover the old description (a forgotten bump); facets' `uses` must bind.
    fn check_providers(&self) -> ChordResult<()> {
        let inner = self.inner();
        let (current, mut facets) = {
            let state = self.host.state();
            let current = state.code_versions.get(&state.current).map(|version| version.services.clone());
            (current.unwrap_or_default(), state.facets.clone())
        };
        for name in &inner.changed {
            let next = &inner.services[name].contract;
            let Some(old) = current.get(name).map(|service| &service.contract) else { continue };
            if old.version() != next.version() {
                continue;
            }
            if let Some(reason) = coverage(next.description(), old.description()) {
                let message =
                    format!("{name}@{} changed incompatibly without a version bump ({reason})", next.version());
                return Err(ChordError::code_only(ChordErrorCode::ContractMismatch, message));
            }
        }
        for facet in &self.facets {
            facets.insert(facet.name().to_string(), facet.clone());
        }
        for name in &self.options.remove {
            facets.shift_remove(name);
        }
        for facet in facets.values() {
            for token in facet.uses() {
                let Some(service) = inner.services.get(token.name()) else { continue };
                if inner.changed.contains(token.name()) {
                    bind(&service.contract, service.options.accepts.as_ref(), token)?;
                }
            }
        }
        Ok(())
    }

    /// Prepares every live object of a changed type, including objects that activate meanwhile, to a fixpoint.
    /// Objects already running the new code (types this reload adds) are skipped.
    async fn prepare_all(self: &Arc<Self>) -> ChordResult<()> {
        loop {
            let objects: Vec<Arc<ObjectImpl>> = self.host.state().objects.values().cloned().collect();
            let (changed, services, prepared) = {
                let inner = self.inner();
                let prepared: HashSet<usize> = inner.candidates.keys().copied().collect();
                (inner.changed.clone(), inner.services.clone(), prepared)
            };
            let todo: Vec<Arc<ObjectImpl>> = objects
                .into_iter()
                .filter(|object| {
                    changed.contains(object.type_name())
                        && !prepared.contains(&key(object))
                        && !runs(object, services.get(object.type_name()))
                        && matches!(object.status(), ObjectStatus::Live | ObjectStatus::Activating)
                })
                .collect();
            if todo.is_empty() {
                return Ok(());
            }
            join_all(todo.iter().map(|object| object.ready())).await;
            self.alive()?;
            for object in todo {
                if object.status() == ObjectStatus::Live && !runs(&object, services.get(object.type_name())) {
                    self.prepare_object(&object, &services[object.type_name()]).await?;
                    self.alive()?;
                }
            }
        }
    }

    /// Runs the new factory in recording mode and opens what can be opened without the gate.
    async fn prepare_object(self: &Arc<Self>, object: &Arc<ObjectImpl>, service: &Arc<Service>) -> ChordResult<()> {
        let earlier = object.decls();
        let run = Arc::new(object.run(service, Some(earlier.clone()))?);
        let rolled_back = {
            let mut inner = self.inner();
            let candidate = Candidate {
                object: object.clone(),
                service: service.clone(),
                run: run.clone(),
                reasons: Vec::new(),
                at_gate: Vec::new(),
                replaced: Vec::new(),
                migrated: Vec::new(),
                derived: HashMap::new(),
                staged: None,
            };
            if !inner.rolled_back {
                inner.candidates.insert(key(object), candidate);
            }
            inner.rolled_back
        };
        if rolled_back {
            object.state().pending.retain(|decls| !Arc::ptr_eq(decls, &run.decls));
            if let Err(error) = object.unwind(&run).await {
                self.host.report(error);
            }
            return self.alive();
        }
        let (reasons, at_gate, replaced) = self.gate_reasons(object, &earlier, &run)?;
        let compatible = reasons.is_empty();
        let opened: Vec<Arc<ResourceImpl>> = {
            let gated: HashSet<usize> = at_gate.iter().map(id).collect();
            run.created.resources.iter().filter(|resource| !gated.contains(&id(resource))).cloned().collect()
        };
        if let Some(candidate) = self.inner().candidates.get_mut(&key(object)) {
            candidate.reasons = reasons;
            candidate.at_gate = at_gate;
            candidate.replaced = replaced;
        }
        let signal = self.abort.signal();
        object.open(&opened, &run.created.refs, &signal).await?;
        object.wait_hydrated(&run).await?;
        // Cells this reload adds read their first value once the new resources are open (gated objects: at the
        // gate); the derivations over them are checked after.
        if compatible {
            object.init_cells(&run, &signal).await?;
            self.dry_run(key(object))?;
        }
        Ok(())
    }

    /// Why an object needs the gate: raised cell versions and replaced exclusive resources. A lowered version, or a
    /// changed schema without a version bump, is refused.
    #[allow(clippy::type_complexity, reason = "three lists of one candidate, filled together")]
    fn gate_reasons(
        &self,
        object: &ObjectImpl,
        earlier: &crate::host2::object::Decls,
        run: &crate::host2::object::Run,
    ) -> ChordResult<(Vec<String>, Vec<Arc<ResourceImpl>>, Vec<Arc<ResourceImpl>>)> {
        let mut reasons = Vec::new();
        {
            let (old, new) = (earlier.data(), run.decls.data());
            for (name, cell) in &new.cells {
                if !old.cells.get(name).is_some_and(|kept| Arc::ptr_eq(kept, cell)) {
                    continue;
                }
                let from = old.cell_versions.get(name).copied().unwrap_or(1);
                let to = new.cell_versions.get(name).copied().unwrap_or(1);
                let address = object.address();
                if to < from {
                    let message = format!("{address}: version of state {name} lowered from {from} to {to}");
                    return Err(invalid("state", message, None));
                }
                if to > from {
                    reasons.push(format!("state {name} version {from} -> {to}"));
                } else if new.cell_schemas.get(name).is_none_or(|schema| !deep_equal(&cell.schema(), schema)) {
                    let message = format!("{address}: schema of state {name} changed without a version bump");
                    return Err(invalid("state", message, None));
                }
            }
        }
        let (mut at_gate, mut replaced): (Vec<Arc<ResourceImpl>>, Vec<Arc<ResourceImpl>>) = (Vec::new(), Vec::new());
        for resource in &run.created.resources {
            let behind_gate = |dep: &Dependency| match dep {
                Dependency::Resource(dep) => at_gate.iter().any(|gated| Arc::ptr_eq(gated, dep)),
                Dependency::Ref(_) => false,
            };
            if resource.exclusive() || resource.deps.iter().any(behind_gate) {
                at_gate.push(resource.clone());
            }
        }
        for resource in &at_gate {
            let old = earlier.data().resources.get(resource.key()).cloned();
            if let Some(old) = old.filter(|old| resource.exclusive() && old.exclusive() && old.is_open()) {
                replaced.push(old);
            }
        }
        if !at_gate.is_empty() {
            reasons.push("exclusive resource replaced".to_string());
        }
        Ok((reasons, at_gate, replaced))
    }

    /// Every reference that survives into the new code version must bind to the new provider. Browser references
    /// do not block a reload: after the commit they get `upgrade_required` instead.
    fn check_refs(&self) -> ChordResult<()> {
        let inner = self.inner();
        let mut dropped: HashSet<usize> = HashSet::new();
        for candidate in inner.candidates.values() {
            let kept: HashSet<usize> = candidate.run.decls.data().refs.values().map(|r| id(r.core())).collect();
            let old = candidate.object.decls();
            dropped.extend(old.data().refs.values().map(|r| id(r.core())).filter(|r| !kept.contains(r)));
        }
        let (objects, host_objects) = {
            let state = self.host.state();
            let pinned: HashMap<String, usize> =
                state.host_objects.iter().map(|(name, r)| (name.clone(), id(r.core()))).collect();
            (state.objects.values().cloned().collect::<Vec<_>>(), pinned)
        };
        for object in objects {
            let Some(service) = inner.services.get(object.type_name()) else { continue };
            if !inner.changed.contains(object.type_name()) {
                continue;
            }
            for reference in object.refs() {
                // The host's own reference follows this provider at commit; it is not an old-code consumer.
                let reference_id = id(&reference);
                if host_objects.get(object.type_name()) == Some(&reference_id) {
                    continue;
                }
                if !dropped.contains(&reference_id) && reference.owner() != RefOwner::Client {
                    bind(&service.contract, service.options.accepts.as_ref(), reference.contract())?;
                }
            }
        }
        Ok(())
    }

    /// Live objects with the registrations they will have after the commit.
    fn entries(&self) -> Vec<(Arc<ObjectImpl>, Vec<Registration>)> {
        let objects: Vec<Arc<ObjectImpl>> = self.host.state().objects.values().cloned().collect();
        let inner = self.inner();
        let live = objects
            .into_iter()
            .filter(|object| object.status() == ObjectStatus::Live && !inner.removed.contains(object.type_name()));
        live.map(|object| {
            let decls = match inner.candidates.get(&key(&object)) {
                Some(candidate) => candidate.run.decls.clone(),
                None => object.decls(),
            };
            let registrations = decls.data().registrations.values().cloned().collect();
            (object, registrations)
        })
        .collect()
    }

    /// Subscribes every new listener now, buffering its events, so a failing source fails prepare instead of
    /// leaving a half-installed code version. Objects running only the new code are staged too.
    fn stage(&self) -> ChordResult<()> {
        let candidates: Vec<(usize, Arc<ObjectImpl>, Arc<crate::host2::object::Run>)> = {
            let inner = self.inner();
            inner.candidates.iter().map(|(key, c)| (*key, c.object.clone(), c.run.clone())).collect()
        };
        for (key, object, run) in candidates {
            if object.status() != ObjectStatus::Live {
                continue;
            }
            let staged = object.stage_listeners(&run.decls)?;
            if let Some(candidate) = self.inner().candidates.get_mut(&key) {
                candidate.staged = Some(staged);
            }
        }
        let objects: Vec<Arc<ObjectImpl>> = self.host.state().objects.values().cloned().collect();
        for object in objects {
            if object.status() == ObjectStatus::Live && object.deferred_runtime() {
                let staged = object.stage_listeners(&object.decls())?;
                self.inner().deferred.push((object, staged));
            }
        }
        Ok(())
    }
}
