//! Port of upstream `cell.ts`: object-owned state sources. A [`Cell`] holds a JSON value; a [`Derived`]
//! projects readables. A write recomputes the dependents of the cell in the same step, then publishes all of them
//! through the publish queue, so a listener never sees a derivation that lags behind its sources.

mod derived;
mod state;

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use indexmap::IndexMap;

pub(crate) use derived::compute_dependents;
pub use derived::{DeriveFn, Derived};
pub use state::Cell;
pub(crate) use state::WriteCheck;

use super::errors::ChordErrorCode;
use super::exec::{ErrorHandler, guarded};
use super::publish::enqueue;
use super::schema::assert_valid;
use super::types::{EventListener, ReadableRef, StateEvent, ValueListener};
use crate::error::{ChordError, ChordResult};
use crate::json::Json;
use crate::sync::{Unsubscribe, lock};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// `not_ready: {what} is not ready`, the error of a value read before activation finished.
pub(crate) fn not_ready(what: &str) -> ChordError {
    ChordError::code_only(ChordErrorCode::NotReady, format!("{what} is not ready"))
}

/// The readables of `sources` that have no value yet.
pub(crate) fn unhydrated<'a>(sources: impl IntoIterator<Item = &'a ReadableRef>) -> Vec<ReadableRef> {
    sources.into_iter().filter(|source| !source.hydrated()).cloned().collect()
}

/// Serializes the writes of one object's sources: a write, its derivations and their publication are one step.
#[derive(Default)]
pub(crate) struct WriteLock {
    mutex: Mutex<()>,
}

thread_local! {
    static WRITING: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// Held while one write computes and adopts its revision. Drop it before the publications drain.
pub(crate) struct WriteGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    id: usize,
}

impl WriteLock {
    pub(crate) fn enter(&self) -> ChordResult<WriteGuard<'_>> {
        let id = self as *const Self as usize;
        if WRITING.with(|writing| writing.borrow().contains(&id)) {
            return Err(ChordError::type_error("state cannot be written inside a change callback or a derivation"));
        }
        let guard = lock(&self.mutex);
        WRITING.with(|writing| writing.borrow_mut().push(id));
        Ok(WriteGuard { _guard: guard, id })
    }
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        WRITING.with(|writing| {
            let mut writing = writing.borrow_mut();
            if let Some(index) = writing.iter().rposition(|id| *id == self.id) {
                writing.remove(index);
            }
        });
    }
}

/// What every source of one object shares.
#[derive(Clone)]
pub(crate) struct SourceOptions {
    pub dev: bool,
    pub report: ErrorHandler,
    pub lock: Arc<WriteLock>,
}

#[derive(Default)]
struct Listeners {
    next: u64,
    values: IndexMap<u64, ValueListener>,
    events: IndexMap<u64, EventListener>,
}

/// Fans out values and delta events; a failing listener is reported, never thrown to the writer.
struct Fanout {
    listeners: Arc<Mutex<Listeners>>,
    report: ErrorHandler,
}

impl Fanout {
    fn add(&self, add: impl FnOnce(&mut Listeners, u64), remove: fn(&mut Listeners, u64)) -> Unsubscribe {
        let id = {
            let mut listeners = lock(&self.listeners);
            listeners.next += 1;
            let id = listeners.next;
            add(&mut listeners, id);
            id
        };
        let listeners: Weak<Mutex<Listeners>> = Arc::downgrade(&self.listeners);
        Unsubscribe::new(move || {
            if let Some(listeners) = listeners.upgrade() {
                remove(&mut lock(&listeners), id);
            }
        })
    }

    fn subscribe(&self, listener: ValueListener, current: Option<Json>) -> Unsubscribe {
        let added = listener.clone();
        let unsubscribe = self.add(
            move |listeners, id| {
                listeners.values.insert(id, added);
            },
            |listeners, id| {
                listeners.values.shift_remove(&id);
            },
        );
        if let Some(current) = current {
            guarded(&self.report, || listener(&current));
        }
        unsubscribe
    }

    fn events(&self, listener: EventListener, current: Option<Json>) -> Unsubscribe {
        let added = listener.clone();
        let unsubscribe = self.add(
            move |listeners, id| {
                listeners.events.insert(id, added);
            },
            |listeners, id| {
                listeners.events.shift_remove(&id);
            },
        );
        if let Some(value) = current {
            guarded(&self.report, || listener(&StateEvent::Snap { value }));
        }
        unsubscribe
    }

    /// Queues delivery to the listeners subscribed now. A later listener got this revision in its snapshot;
    /// one that unsubscribes before delivery is skipped.
    fn emit(&self, event: StateEvent) {
        let (events, values) = {
            let listeners = lock(&self.listeners);
            let events: Vec<_> = listeners.events.iter().map(|(id, listener)| (*id, listener.clone())).collect();
            let values: Vec<_> = listeners.values.iter().map(|(id, listener)| (*id, listener.clone())).collect();
            (events, values)
        };
        let (listeners, report) = (self.listeners.clone(), self.report.clone());
        enqueue(move || {
            for (id, listener) in events {
                if lock(&listeners).events.contains_key(&id) {
                    guarded(&report, || listener(&event));
                }
            }
            for (id, listener) in values {
                if lock(&listeners).values.contains_key(&id) {
                    guarded(&report, || listener(event.value()));
                }
            }
        });
    }
}

/// The shared part of a cell and a derivation: key, order, dependents and the schemas it is exposed under.
pub struct SourceCore {
    key: String,
    seq: u64,
    dependents: Mutex<Vec<Weak<Derived>>>,
    /// Schemas of remote state members exposing this source now (checked per write in dev mode).
    schemas: Mutex<Vec<Json>>,
    options: SourceOptions,
    fanout: Fanout,
}

impl SourceCore {
    pub(crate) fn new(key: String, options: SourceOptions) -> Self {
        let fanout = Fanout { listeners: Arc::default(), report: options.report.clone() };
        let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
        Self { key, seq, dependents: Mutex::default(), schemas: Mutex::default(), options, fanout }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn id(&self) -> usize {
        self as *const Self as usize
    }

    pub(crate) fn options(&self) -> &SourceOptions {
        &self.options
    }

    pub(crate) fn set_schemas(&self, schemas: Vec<Json>) {
        *lock(&self.schemas) = schemas;
    }

    /// Per-write validation, dev mode only: `own` is a cell's schema.
    pub(crate) fn validate(&self, own: Option<&Json>, value: &Json) -> ChordResult<()> {
        if !self.options.dev {
            return Ok(());
        }
        let what = format!("state {}", self.key);
        let schemas = lock(&self.schemas).clone();
        for schema in own.into_iter().chain(schemas.iter()) {
            assert_valid("state", &what, schema, Some(value))?;
        }
        Ok(())
    }

    fn add_dependent(&self, derived: &Weak<Derived>) {
        let mut dependents = lock(&self.dependents);
        if !dependents.iter().any(|existing| existing.ptr_eq(derived)) {
            dependents.push(derived.clone());
        }
    }

    fn remove_dependent(&self, derived: &Weak<Derived>) {
        lock(&self.dependents).retain(|existing| !existing.ptr_eq(derived) && existing.strong_count() > 0);
    }

    fn dependents(&self) -> Vec<Arc<Derived>> {
        lock(&self.dependents).iter().filter_map(Weak::upgrade).collect()
    }

    fn subscribe(&self, listener: ValueListener, current: Option<Json>) -> Unsubscribe {
        self.fanout.subscribe(listener, current)
    }

    fn events(&self, listener: EventListener, current: Option<Json>) -> Unsubscribe {
        self.fanout.events(listener, current)
    }

    fn emit(&self, event: StateEvent) {
        self.fanout.emit(event);
    }
}
