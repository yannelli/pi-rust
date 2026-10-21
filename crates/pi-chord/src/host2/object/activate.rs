//! Activation as one transaction: factory, opens, then a synchronous finalization (registry check,
//! implementations, runtime). Every failure unwinds everything the activation did.

use std::collections::HashMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Weak};

use futures::FutureExt;
use futures::future::{join_all, try_join_all};

use super::declare::{Declare, Decls, Run};
use super::service::validate_implementation;
use super::{Installed, Member, ObjectImpl, ObjectStatus, Ref, ResourceImpl, RootOptions, Service, run_root};
use crate::context::AbortSignal;
use crate::error::{ChordError, ChordResult};
use crate::host2::cell::unhydrated;
use crate::host2::declare::Dependency;
use crate::host2::errors::{ChordErrorCode, as_chord_error, run_all};
use crate::host2::exec::{FactoryFrame, panic_error, spawn_later, spawn_now};
use crate::host2::types::{Readable, ReadableRef, RefStatus};
use crate::sync::{BoxFuture, SharedTask, lock};

/// The activation of the object `me` refers to, as a shared task; it runs once [`ObjectImpl::launch`] drives it.
pub(super) fn activation(me: Weak<ObjectImpl>, after: Option<SharedTask>) -> SharedTask {
    let activation: BoxFuture<ChordResult<()>> = Box::pin(async move {
        let Some(object) = me.upgrade() else { return Err(ChordError::error("object dropped before activation")) };
        match object.activate(after).await {
            Ok(()) => {
                for reference in object.refs() {
                    reference.live();
                }
                object.maybe_idle();
                Ok(())
            }
            Err(cause) => {
                {
                    let mut state = object.state();
                    if state.status == ObjectStatus::Activating {
                        state.status = ObjectStatus::Failed;
                    }
                }
                object.host.forget(&object);
                // A coded error (forbidden, a dependency's error, ...) keeps its code; anything else is `unavailable`.
                let error = as_chord_error(cause, &format!("activating {}", object.address()));
                let refs = std::mem::take(&mut object.state().refs);
                for reference in refs.values() {
                    reference.gone(&error);
                }
                Err(error)
            }
        }
    });
    activation.shared()
}

fn abandoned(object: &ObjectImpl) -> ChordError {
    ChordError::code_only(ChordErrorCode::Aborted, format!("{} abandoned", object.address()))
}

impl ObjectImpl {
    /// Starts the activation (after the caller's synchronous code, like upstream's microtask).
    pub(crate) fn launch(&self) {
        let ready = self.ready();
        spawn_later(self.host.handle(), async move {
            let _ = ready.await;
        });
    }

    async fn activate(self: &Arc<Self>, after: Option<SharedTask>) -> ChordResult<()> {
        if let Some(after) = after {
            let _ = after.await;
        }
        let signal = self.activation_abort.signal();
        if signal.aborted() {
            return Err(abandoned(self));
        }
        let Some(service) = self.host.service(self.type_name()) else {
            let message = format!("{} is not served", self.type_name());
            return Err(ChordError::with_reason(ChordErrorCode::Unavailable, "unknown_type", message));
        };
        self.state().service = Some(service.clone());
        let run = self.run(&service, None)?;
        let result = self.finish_activation(&service, &run, &signal).await;
        if let Err(error) = &result {
            self.roll_back_activation(&run, error).await;
        }
        self.state().pending.retain(|decls| !Arc::ptr_eq(decls, &run.decls));
        result
    }

    async fn finish_activation(
        self: &Arc<Self>,
        service: &Arc<Service>,
        run: &Run,
        signal: &AbortSignal,
    ) -> ChordResult<()> {
        self.open(&run.created.resources, &run.created.refs, signal).await?;
        if signal.aborted() {
            return Err(abandoned(self));
        }
        self.wait_hydrated(run).await?;
        if signal.aborted() {
            return Err(abandoned(self));
        }
        self.init_cells(run, signal).await?;
        if signal.aborted() {
            return Err(abandoned(self));
        }
        self.host.check_registrations(self, &run.decls)?;
        let current = self.host.current_code_version();
        let pending = self.host.pending_code_version();
        let installed = |code_version| Installed {
            code_version,
            service: service.clone(),
            implementation: run.implementation.clone(),
            decls: run.decls.clone(),
        };
        let retained = self.host.retained_services(self.type_name());
        {
            let mut state = self.state();
            state.decls = run.decls.clone();
            for (code_version, served) in retained {
                if served.is_some_and(|served| Arc::ptr_eq(&served, service)) {
                    state.implementations.insert(code_version, installed(code_version));
                }
            }
            let serves = |code_version| state.implementations.contains_key(&code_version);
            if !serves(current) && pending.is_none_or(|pending| !serves(pending)) {
                let message = format!("{}: code changed during activation", self.address());
                return Err(ChordError::code_only(ChordErrorCode::Superseded, message));
            }
        }
        for derived in &run.created.derives {
            derived.attach(None)?;
        }
        self.refresh_schemas();
        // Served only by a running reload's pending code version: new code must not run before that reload commits.
        let deferred = !self.state().implementations.contains_key(&current);
        self.state().deferred_runtime = deferred;
        if !deferred {
            if !run.decls.data().registrations.is_empty() {
                self.host.registries_changed(Some(self));
            }
            self.start_runtime()?;
        }
        // A listener starting up may have released the last reference synchronously: no resurrection.
        let mut state = self.state();
        if signal.aborted() || state.status != ObjectStatus::Activating {
            return Err(abandoned(self));
        }
        state.status = ObjectStatus::Live;
        Ok(())
    }

    async fn roll_back_activation(self: &Arc<Self>, run: &Run, error: &ChordError) {
        // Invocations started during finalization (listener roots) end before their resources close.
        self.call_abort.abort(Some(error.clone()));
        self.stop_listeners();
        self.stop_tasks();
        let registered = {
            let mut state = self.state();
            let registered = !state.decls.data().registrations.is_empty() && !state.deferred_runtime;
            state.implementations.clear();
            state.decls = Arc::default();
            registered
        };
        if registered {
            self.host.registries_changed(None);
        }
        self.wait_invocations().await;
        let stopped: Vec<SharedTask> = self.state().stopped.values().cloned().collect();
        join_all(stopped).await;
        if let Err(cleanup) = self.unwind(run).await {
            self.host.report(cleanup);
        }
    }

    /// Runs the factory synchronously. With `earlier`, reruns it for a reload (phase 4). The run's
    /// declarations stay in `pending` (seen by the cycle check) until the caller installs or discards them.
    pub(crate) fn run(self: &Arc<Self>, service: &Service, earlier: Option<Arc<Decls>>) -> ChordResult<Run> {
        let mut declare = Declare::new(self.clone(), earlier);
        self.state().pending.push(declare.decls().clone());
        let frame = FactoryFrame::enter(self.host.id());
        let produced =
            catch_unwind(AssertUnwindSafe(|| (service.factory)(self.input(), self.principal(), &mut declare)));
        let misuse = frame.take_misuse();
        drop(frame);
        let produced = match produced {
            Ok(produced) => produced,
            Err(panic) => Err(ChordError::error(format!("factory {}", panic_error(panic.as_ref())))),
        };
        let checked = match produced {
            Ok(implementation) => {
                let run = declare.into_run(implementation);
                match misuse.map_or_else(|| validate_implementation(service, &run.implementation), Err) {
                    Ok(()) => return Ok(run),
                    Err(error) => (run, error),
                }
            }
            Err(error) => (declare.into_run(Default::default()), error),
        };
        let (run, error) = checked;
        self.discard(run);
        Err(error)
    }

    fn discard(self: &Arc<Self>, run: Run) {
        self.state().pending.retain(|decls| !Arc::ptr_eq(decls, &run.decls));
        let object = self.clone();
        spawn_now(self.host.handle(), async move {
            if let Err(cleanup) = object.unwind(&run).await {
                object.host.report(cleanup);
            }
        });
    }

    /// Opens resources (dependencies first) and waits for references; settles all before failing.
    pub(crate) async fn open(
        self: &Arc<Self>,
        resources: &[Arc<ResourceImpl>],
        refs: &[Ref],
        signal: &AbortSignal,
    ) -> ChordResult<()> {
        let mut opening: HashMap<usize, SharedTask> = HashMap::new();
        let mut work: Vec<BoxFuture<ChordResult<()>>> = Vec::new();
        for resource in resources {
            work.push(Box::pin(self.open_one(resource, resources, signal, &mut opening)));
        }
        let waiting = refs.to_vec();
        let ready = try_join_all(refs.iter().map(|reference| reference.ready()));
        let object = self.clone();
        work.push(Box::pin(async move {
            let waiting = move || {
                let loading = waiting.iter().filter(|reference| reference.status() == RefStatus::Loading);
                loading.map(|reference| reference.address().to_string()).collect()
            };
            object.within_ready_timeout(ready, waiting).await.map(|_| ())
        }));
        join_all(work).await.into_iter().find(Result::is_err).unwrap_or(Ok(()))
    }

    fn open_one(
        self: &Arc<Self>,
        resource: &Arc<ResourceImpl>,
        created: &[Arc<ResourceImpl>],
        signal: &AbortSignal,
        opening: &mut HashMap<usize, SharedTask>,
    ) -> SharedTask {
        let id = Arc::as_ptr(resource) as usize;
        if let Some(task) = opening.get(&id) {
            return task.clone();
        }
        let mut deps: Vec<BoxFuture<ChordResult<()>>> = Vec::new();
        for dep in &resource.deps {
            match dep {
                Dependency::Ref(reference) => deps.push(reference.ready()),
                Dependency::Resource(dep) if created.iter().any(|created| Arc::ptr_eq(created, dep)) => {
                    deps.push(Box::pin(self.open_one(dep, created, signal, opening)));
                }
                Dependency::Resource(_) => {}
            }
        }
        let (object, resource, signal) = (self.clone(), resource.clone(), signal.clone());
        let task: BoxFuture<ChordResult<()>> = Box::pin(async move {
            try_join_all(deps).await?;
            resource.open(&signal, &object.open_order).await
        });
        let task = task.shared();
        opening.insert(id, task.clone());
        task
    }

    /// Waits until the state sources a run exposes or derives from have their values.
    async fn wait_hydrated(&self, run: &Run) -> ChordResult<()> {
        let exposed: Vec<(String, ReadableRef)> = run
            .implementation
            .members()
            .filter_map(|(name, member)| match member {
                Member::State(readable) => Some((name.to_string(), readable.clone())),
                _ => None,
            })
            .collect();
        let derives: Vec<ReadableRef> =
            run.created.derives.iter().map(|derived| derived.clone() as ReadableRef).collect();
        let sources: Vec<ReadableRef> = derives.iter().cloned().chain(exposed.iter().map(|(_, r)| r.clone())).collect();
        let pending = unhydrated(&sources);
        if pending.is_empty() {
            return Ok(());
        }
        let work = try_join_all(pending.iter().map(|source| source.when_hydrated()));
        let (address, derived) = (self.address().to_string(), run.created.derives.clone());
        let waiting = move || {
            let members = exposed.iter().filter(|(_, readable)| !readable.hydrated());
            let members = members.map(|(name, _)| format!("{address}.{name}"));
            let derives = derived.iter().filter(|derived| !derived.hydrated()).map(|d| format!("derive {}", d.key()));
            members.chain(derives).collect()
        };
        self.within_ready_timeout(work, waiting).await.map(|_| ())
    }

    /// `work` within the host's ready timeout; a timeout names what is still awaited.
    async fn within_ready_timeout<T>(
        &self,
        work: impl Future<Output = ChordResult<T>>,
        waiting: impl FnOnce() -> Vec<String>,
    ) -> ChordResult<T> {
        let Some(timeout) = self.host.ready_timeout() else { return work.await };
        match tokio::time::timeout(timeout, work).await {
            Ok(result) => result,
            Err(_) => {
                let list = waiting();
                let list = if list.is_empty() { String::new() } else { format!(", waiting on {}", list.join(", ")) };
                let message = format!(
                    "{} was not ready after {} ms{list} (a reference cycle across hosts?)",
                    self.address(),
                    timeout.as_millis()
                );
                Err(ChordError::with_reason(ChordErrorCode::Unavailable, "ready_timeout", message))
            }
        }
    }

    /// Fills the cells `run` created with `init`, each in a root of its own, in parallel; settles all first.
    async fn init_cells(&self, run: &Run, signal: &AbortSignal) -> ChordResult<()> {
        let work = run.created.inits.iter().map(|(cell, init)| {
            let options = RootOptions { signal: Some(signal.clone()), ..RootOptions::default() };
            let init = init.clone();
            let root = run_root(&self.host, options, move |ctx| init(ctx));
            let (cell, signal) = (cell.clone(), signal.clone());
            async move {
                let value = root.await?;
                if !signal.aborted() {
                    cell.initialize(value)?;
                }
                Ok(())
            }
        });
        join_all(work).await.into_iter().find(Result::is_err).unwrap_or(Ok(()))
    }

    /// Undoes the objects a run created: closes its resources (reverse order) and releases its references.
    pub(crate) async fn unwind(self: &Arc<Self>, run: &Run) -> ChordResult<()> {
        for derived in &run.created.derives {
            derived.detach();
        }
        let order = lock(&self.open_order).clone();
        let position = |resource: &Arc<ResourceImpl>| order.iter().position(|open| Arc::ptr_eq(open, resource));
        let mut resources = run.created.resources.clone();
        resources.sort_by_key(|resource| std::cmp::Reverse(position(resource).map_or(-1, |index| index as i64)));
        let mut steps: Vec<BoxFuture<ChordResult<()>>> = Vec::new();
        for resource in resources {
            let object = self.clone();
            steps.push(Box::pin(async move { resource.close(&object.open_order).await }));
        }
        for reference in run.created.refs.clone() {
            steps.push(Box::pin(async move {
                reference.release();
                Ok(())
            }));
        }
        run_all(steps, &format!("{}: cleanup failed", self.address())).await
    }
}
