//! Keyed instance lifetime and the cancellable tasks watching those instances.

use indexmap::IndexMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::handle::{LocalImpl, ServiceHandle};
use crate::context::{AbortController, BACKGROUND_CONTEXT, Context, with_cancel};
use crate::error::{ChordError, ChordResult};
use crate::state::ErrorReporter;
use crate::sync::{BoxFuture, lock, spawn_eager};

/// The service a keyed instance exposes to watchers.
#[derive(Clone)]
pub(crate) enum InstanceService {
    Remote(ServiceHandle),
    Local(LocalImpl),
}

pub(crate) type InstanceHandler = Arc<dyn Fn(InstanceService, Context) -> BoxFuture<ChordResult<()>> + Send + Sync>;

pub(crate) struct InstanceEntry {
    pub key: String,
    pub generation: u64,
    pub service: InstanceService,
    /// The consumer facade that receives updates, for remote instances.
    pub facade: Option<Arc<super::consumer::facade::ServiceFacade>>,
    pub deactivate: Box<dyn Fn() + Send + Sync>,
}

struct Observer {
    handler: InstanceHandler,
    tasks: Mutex<Vec<(Arc<InstanceEntry>, AbortController)>>,
    closed: AtomicBool,
}

struct State {
    /// Insertion order, like a JavaScript `Map`, so observers start in snapshot order.
    entries: IndexMap<String, Arc<InstanceEntry>>,
    observers: Vec<Arc<Observer>>,
    ready: bool,
    disposed: bool,
}

pub(crate) struct InstanceDirectory {
    state: Mutex<State>,
    report_error: ErrorReporter,
}

impl InstanceDirectory {
    pub fn new(ready: bool, report_error: ErrorReporter) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State { entries: IndexMap::new(), observers: Vec::new(), ready, disposed: false }),
            report_error,
        })
    }

    pub fn observer_count(&self) -> usize {
        lock(&self.state).observers.len()
    }

    pub fn values(&self) -> Vec<Arc<InstanceEntry>> {
        lock(&self.state).entries.values().cloned().collect()
    }

    pub fn get(&self, key: &str) -> Option<Arc<InstanceEntry>> {
        lock(&self.state).entries.get(key).cloned()
    }

    fn assert_active(state: &State) -> ChordResult<()> {
        if state.disposed {
            return Err(ChordError::error("Keyed service directory is disposed"));
        }
        Ok(())
    }

    pub fn insert(&self, entry: Arc<InstanceEntry>) -> ChordResult<()> {
        let observers = {
            let mut state = lock(&self.state);
            Self::assert_active(&state)?;
            if state.entries.contains_key(&entry.key) {
                let key = &entry.key;
                return Err(ChordError::error(format!("Keyed service already has a live instance with key {key}")));
            }
            state.entries.insert(entry.key.clone(), entry.clone());
            if state.ready { state.observers.clone() } else { Vec::new() }
        };
        for observer in observers {
            self.start(&observer, &entry);
        }
        Ok(())
    }

    pub fn replace(&self, entry: Arc<InstanceEntry>) -> ChordResult<()> {
        let previous = {
            let state = lock(&self.state);
            Self::assert_active(&state)?;
            state.entries.get(&entry.key).cloned()
        };
        if let Some(previous) = previous {
            if previous.generation == entry.generation {
                return Err(ChordError::error("Keyed service repeated a live generation"));
            }
            self.remove(&previous);
        }
        let observers = {
            let mut state = lock(&self.state);
            state.entries.insert(entry.key.clone(), entry.clone());
            if state.ready { state.observers.clone() } else { Vec::new() }
        };
        for observer in observers {
            self.start(&observer, &entry);
        }
        Ok(())
    }

    pub fn remove(&self, entry: &Arc<InstanceEntry>) {
        let observers = {
            let mut state = lock(&self.state);
            match state.entries.get(&entry.key) {
                Some(current) if Arc::ptr_eq(current, entry) => {}
                _ => return,
            }
            state.entries.shift_remove(&entry.key);
            state.observers.clone()
        };
        (entry.deactivate)();
        for observer in observers {
            let mut tasks = lock(&observer.tasks);
            if let Some(index) = tasks.iter().position(|(task, _)| Arc::ptr_eq(task, entry)) {
                let (_, controller) = tasks.remove(index);
                drop(tasks);
                controller.abort(None);
            }
        }
    }

    pub fn ready(&self) -> ChordResult<()> {
        let (entries, observers) = {
            let mut state = lock(&self.state);
            Self::assert_active(&state)?;
            if state.ready {
                return Ok(());
            }
            state.ready = true;
            (state.entries.values().cloned().collect::<Vec<_>>(), state.observers.clone())
        };
        for entry in &entries {
            for observer in &observers {
                self.start(observer, entry);
            }
        }
        Ok(())
    }

    pub fn reset(&self) {
        let entries = {
            let mut state = lock(&self.state);
            if state.disposed {
                return;
            }
            state.ready = false;
            state.entries.values().cloned().collect::<Vec<_>>()
        };
        for entry in entries {
            self.remove(&entry);
        }
    }

    /// Start a task for each live instance, now when ready or once ready.
    pub fn observe(self: &Arc<Self>, handler: InstanceHandler) -> ChordResult<crate::sync::Unsubscribe> {
        let observer = Arc::new(Observer { handler, tasks: Mutex::new(Vec::new()), closed: AtomicBool::new(false) });
        let entries = {
            let mut state = lock(&self.state);
            Self::assert_active(&state)?;
            state.observers.push(observer.clone());
            if state.ready { state.entries.values().cloned().collect() } else { Vec::new() }
        };
        for entry in &entries {
            self.start(&observer, entry);
        }
        let directory = Arc::downgrade(self);
        Ok(crate::sync::Unsubscribe::new(move || {
            if observer.closed.swap(true, Ordering::SeqCst) {
                return;
            }
            let tasks = std::mem::take(&mut *lock(&observer.tasks));
            for (_, controller) in tasks {
                controller.abort(None);
            }
            if let Some(directory) = directory.upgrade() {
                lock(&directory.state).observers.retain(|other| !Arc::ptr_eq(other, &observer));
            }
        }))
    }

    pub fn dispose(&self) {
        let (observers, entries) = {
            let mut state = lock(&self.state);
            if state.disposed {
                return;
            }
            state.disposed = true;
            let entries: Vec<_> = state.entries.drain(..).map(|(_, entry)| entry).collect();
            (std::mem::take(&mut state.observers), entries)
        };
        for observer in observers {
            observer.closed.store(true, Ordering::SeqCst);
            let tasks = std::mem::take(&mut *lock(&observer.tasks));
            for (_, controller) in tasks {
                controller.abort(None);
            }
        }
        for entry in entries {
            (entry.deactivate)();
        }
    }

    fn start(&self, observer: &Arc<Observer>, entry: &Arc<InstanceEntry>) {
        if observer.closed.load(Ordering::SeqCst) {
            return;
        }
        let (context, controller) = with_cancel(&BACKGROUND_CONTEXT);
        {
            let mut tasks = lock(&observer.tasks);
            if tasks.iter().any(|(task, _)| Arc::ptr_eq(task, entry)) {
                return;
            }
            tasks.push((entry.clone(), controller));
        }
        let task = (observer.handler)(entry.service.clone(), context.clone());
        let report = self.report_error.clone();
        spawn_eager(async move {
            if let Err(error) = task.await
                && !context.is_cancelled()
            {
                report(error);
            }
        });
    }
}
