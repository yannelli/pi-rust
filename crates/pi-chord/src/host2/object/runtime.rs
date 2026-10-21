//! The runtime of the current declarations: listener subscriptions and tasks.

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use futures::FutureExt;

use super::declare::{Decls, Listen, Listenable, ListenerDecl};
use super::{Ctx, CtxFields, ObjectImpl, ObjectStatus, RootOptions, RunningTask, run_root};
use crate::context::AbortController;
use crate::error::{ChordError, ChordResult};
use crate::host2::declare::EventSource;
use crate::host2::errors::{ChordErrorCode, invalid};
use crate::host2::exec::{panic_error, spawn_later, spawn_now};
use crate::host2::publish::enqueue;
use crate::host2::types::ReadableRef;
use crate::json::Json;
use crate::sync::{BoxFuture, SharedTask, Unsubscribe, lock};

type Slot = Arc<Mutex<Option<Unsubscribe>>>;

/// One listener's subscription: what it was declared on and how it unsubscribes.
pub(crate) struct Subscription {
    /// The resource the listener was declared on, or its source itself.
    origin: usize,
    select: Option<String>,
    unsubscribe: Slot,
}

impl Subscription {
    fn stop(&self) {
        let unsubscribe = lock(&self.unsubscribe).take();
        if let Some(unsubscribe) = unsubscribe {
            unsubscribe.unsubscribe();
        }
    }
}

struct Fresh {
    key: String,
    subscription: Subscription,
    /// Events of an event source until `activate`; `None` once delivered directly.
    buffer: Arc<Mutex<Option<Vec<Json>>>>,
    /// State (a readable) is subscribed at `activate`: it delivers the value current then.
    state: Option<ReadableRef>,
}

/// Listener subscriptions made ahead of a commit: `activate` switches over, `discard` drops them.
pub(crate) struct StagedListeners {
    object: Arc<ObjectImpl>,
    fresh: Vec<Fresh>,
    kept: HashSet<String>,
}

impl StagedListeners {
    pub(crate) fn activate(self) {
        let replaced: Vec<Subscription> = {
            let mut state = self.object.state();
            let keys: Vec<String> =
                state.subscriptions.keys().filter(|key| !self.kept.contains(*key)).cloned().collect();
            keys.iter().filter_map(|key| state.subscriptions.shift_remove(key)).collect()
        };
        replaced.iter().for_each(Subscription::stop);
        for fresh in self.fresh {
            let slot = fresh.subscription.unsubscribe.clone();
            self.object.state().subscriptions.insert(fresh.key.clone(), fresh.subscription);
            if let Some(readable) = fresh.state {
                let (object, key) = (Arc::downgrade(&self.object), fresh.key.clone());
                let unsubscribe = readable.subscribe(Arc::new(move |value: &Json| {
                    if let Some(object) = object.upgrade() {
                        object.deliver(key.clone(), value.clone());
                    }
                }));
                *lock(&slot) = Some(unsubscribe);
            }
            let buffered = lock(&fresh.buffer).take().unwrap_or_default();
            for event in buffered {
                self.object.deliver(fresh.key.clone(), event);
            }
        }
    }

    pub(crate) fn discard(self) {
        self.fresh.iter().for_each(|fresh| fresh.subscription.stop());
    }
}

impl ObjectImpl {
    /// Resolves a listener's subscribable: `select(resource value)`, the resource's value, or the source itself.
    fn listen_source(&self, key: &str, decl: &ListenerDecl) -> ChordResult<Listenable> {
        let resource = match &decl.source {
            Listen::State(readable) => return Ok(Listenable::State(readable.clone())),
            Listen::Events(source) => return Ok(Listenable::Events(source.clone())),
            Listen::Resource(resource) => resource,
        };
        let value = resource.value()?;
        if let Some(select) = &decl.select {
            return (select.select)(&value);
        }
        if let Some(source) = value.downcast_ref::<EventSource>() {
            return Ok(Listenable::Events(source.clone()));
        }
        if let Some(readable) = value.downcast_ref::<ReadableRef>() {
            return Ok(Listenable::State(readable.clone()));
        }
        Err(invalid("implementation", format!("listener {key}: source has no subscribe()"), None))
    }

    /// Subscribes the listeners of `decls` without delivering: events wait until `activate`. A listener on the
    /// same origin with the same `select` key keeps its subscription. Fails with nothing left subscribed.
    pub(crate) fn stage_listeners(self: &Arc<Self>, decls: &Decls) -> ChordResult<StagedListeners> {
        let listeners: Vec<(String, ListenerDecl)> =
            decls.data().listeners.iter().map(|(key, decl)| (key.clone(), decl.clone())).collect();
        let mut staged = StagedListeners { object: self.clone(), fresh: Vec::new(), kept: HashSet::new() };
        for (key, decl) in listeners {
            let origin = decl.origin();
            let select = decl.select_key().map(str::to_string);
            let same = {
                let state = self.state();
                let current = state.subscriptions.get(&key);
                current.is_some_and(|current| current.origin == origin && current.select == select)
            };
            if same {
                staged.kept.insert(key);
                continue;
            }
            let source = match self.listen_source(&key, &decl) {
                Ok(source) => source,
                Err(error) => {
                    staged.discard();
                    return Err(error);
                }
            };
            let subscription = Subscription { origin, select, unsubscribe: Slot::default() };
            let mut fresh = Fresh { key: key.clone(), subscription, buffer: Arc::default(), state: None };
            match source {
                Listenable::State(readable) => fresh.state = Some(readable),
                Listenable::Events(source) => {
                    *lock(&fresh.buffer) = Some(Vec::new());
                    let (object, buffer) = (Arc::downgrade(self), fresh.buffer.clone());
                    let unsubscribe = source.subscribe(Box::new(move |event: &Json| {
                        let mut buffered = lock(&buffer);
                        match buffered.as_mut() {
                            Some(buffered) => buffered.push(event.clone()),
                            None => {
                                drop(buffered);
                                if let Some(object) = object.upgrade() {
                                    object.deliver(key.clone(), event.clone());
                                }
                            }
                        }
                    }));
                    *lock(&fresh.subscription.unsubscribe) = Some(unsubscribe);
                }
            }
            staged.fresh.push(fresh);
        }
        Ok(staged)
    }

    pub(crate) fn start_runtime(self: &Arc<Self>) -> ChordResult<()> {
        let decls = self.decls();
        self.stage_listeners(&decls)?.activate();
        self.start_tasks();
        Ok(())
    }

    /// Starts the tasks of the current declarations, once the runs `stop_tasks` aborted finished (or after the
    /// host's task stop deadline, reported). A later start or stop, or the object leaving `Live`, cancels it.
    pub(crate) fn start_tasks(self: &Arc<Self>) {
        let (run, stopped) = {
            let mut state = self.state();
            state.task_runs += 1;
            (state.task_runs, state.stopped.values().cloned().collect::<Vec<SharedTask>>())
        };
        if stopped.is_empty() {
            self.start_all_tasks();
            return;
        }
        let deadline = self.host.task_stop_deadline();
        let object = self.clone();
        spawn_later(self.host.handle(), async move {
            let missed = tokio::time::timeout(deadline, futures::future::join_all(stopped)).await.is_err();
            {
                let state = object.state();
                if run != state.task_runs || state.status != ObjectStatus::Live {
                    return;
                }
            }
            if missed {
                let message = format!(
                    "{}: aborted tasks still running after {} ms; starting their replacements anyway",
                    object.address(),
                    deadline.as_millis()
                );
                object.host.report(ChordError::code_only(ChordErrorCode::Aborted, message));
            }
            object.start_all_tasks();
        });
    }

    fn start_all_tasks(self: &Arc<Self>) {
        let keys: Vec<String> = self.decls().data().tasks.keys().cloned().collect();
        for key in keys {
            self.start_task(&key, self.host.current_code_version());
        }
    }

    fn start_task(self: &Arc<Self>, key: &str, code_version: i64) {
        let Some(task) = self.decls().data().tasks.get(key).cloned() else { return };
        let controller = AbortController::new();
        let fields = CtxFields {
            signals: vec![controller.signal(), self.call_abort.signal()],
            root_id: format!("task:{}:{key}", self.address()),
            code_version,
            principal: self.host.principal().clone(),
            holder: true,
        };
        let ctx = Ctx::new(self.host.clone(), fields);
        let (object, key, signal) = (Arc::downgrade(self), key.to_string(), controller.signal());
        let entry = key.clone();
        let done: BoxFuture<ChordResult<()>> = Box::pin(async move {
            let result = AssertUnwindSafe(task(ctx.clone())).catch_unwind().await;
            let result = result.unwrap_or_else(|panic| Err(panic_error(panic.as_ref())));
            let object = object.upgrade();
            if let (Err(error), Some(object)) = (result, &object)
                && !ctx.signal().aborted()
            {
                object.host.report(error);
            }
            ctx.close();
            if let Some(object) = object {
                let mut state = object.state();
                if state.tasks.get(&key).is_some_and(|task| task.controller.signal().ptr_eq(&signal)) {
                    state.tasks.shift_remove(&key);
                }
            }
            Ok(())
        });
        let done = done.shared();
        self.state().tasks.insert(entry, RunningTask { controller, done: done.clone() });
        spawn_now(self.host.handle(), async move {
            let _ = done.await;
        });
    }

    /// Aborts running tasks (and cancels a delayed start). The next `start_tasks` and the disposal wait for them.
    pub(crate) fn stop_tasks(self: &Arc<Self>) {
        let tasks = {
            let mut state = self.state();
            state.task_runs += 1;
            std::mem::take(&mut state.tasks)
        };
        for task in tasks.into_values() {
            task.controller.abort(Some(ChordError::code_only(ChordErrorCode::Aborted, "task stopped")));
            let seq = {
                let mut state = self.state();
                state.stopped_seq += 1;
                let seq = state.stopped_seq;
                state.stopped.insert(seq, task.done.clone());
                seq
            };
            let object = Arc::downgrade(self);
            spawn_now(self.host.handle(), async move {
                let _ = task.done.await;
                if let Some(object) = object.upgrade() {
                    object.state().stopped.shift_remove(&seq);
                }
            });
        }
    }

    pub(crate) fn stop_listeners(&self) {
        let subscriptions = std::mem::take(&mut self.state().subscriptions);
        subscriptions.values().for_each(Subscription::stop);
    }

    /// Delivers one listener event, in publication order, in a root that leases this object, counts as an
    /// invocation and is cancelled by forced disposal.
    pub(crate) fn deliver(self: &Arc<Self>, key: String, event: Json) {
        let object = Arc::downgrade(self);
        enqueue(move || {
            let Some(object) = object.upgrade() else { return };
            if let Some(gate) = &mut object.state().gate {
                gate.queue.push((key, event));
                return;
            }
            let Some(decl) = object.decls().data().listeners.get(&key).cloned() else { return };
            if !matches!(object.status(), ObjectStatus::Live | ObjectStatus::Activating) {
                return;
            }
            let target = object.clone();
            let invocation = run_root(&object.host, RootOptions::default(), move |root| async move {
                target.lease(&root);
                let child = root.child(&target);
                let result = target.invoke((decl.handler)(event, child.clone())).await;
                child.close();
                result
            });
            let host = object.host.clone();
            spawn_now(object.host.handle(), async move {
                if let Err(error) = invocation.await {
                    host.report(error);
                }
            });
        });
    }

    /// Opens the object's gate and replays queued listener events through the current listeners.
    pub(crate) fn open_gate(self: &Arc<Self>) {
        let gate = self.state().gate.take();
        for (key, event) in gate.map(|gate| gate.queue).unwrap_or_default() {
            self.deliver(key, event);
        }
    }
}
