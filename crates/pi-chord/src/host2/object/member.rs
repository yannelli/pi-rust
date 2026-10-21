//! A reference's state member: a stable readable that follows whatever source the current implementation
//! exposes under its name. Rebinding (reload, variant) delivers a reset; upstream is followed only while
//! someone listens.

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use indexmap::IndexMap;

use super::{Member, RefCore, RefImpl};
use crate::error::{ChordError, ChordResult};
use crate::host2::cell::not_ready;
use crate::host2::errors::ChordErrorCode;
use crate::host2::exec::panic_error;
use crate::host2::publish::enqueue;
use crate::host2::types::{EventListener, Readable, ReadableRef, RefStatus, StateEvent, ValueListener, readable_id};
use crate::json::Json;
use crate::sync::{BoxFuture, Unsubscribe, lock};

#[derive(Default)]
struct MemberState {
    next: u64,
    values: IndexMap<u64, ValueListener>,
    events: IndexMap<u64, EventListener>,
    /// Event listeners that have not had their snapshot yet (added while the member was not readable).
    unseen: HashSet<u64>,
    source: Option<ReadableRef>,
    /// Bumped at every rebind: events of an older upstream subscription are dropped.
    generation: u64,
    upstream: Option<Unsubscribe>,
    /// The value listeners last saw; a first upstream snapshot equal to it is not announced again.
    seen: Option<Json>,
}

pub(crate) struct MemberReadable {
    reference: Weak<RefImpl>,
    name: String,
    state: Mutex<MemberState>,
    me: Weak<MemberReadable>,
}

impl MemberReadable {
    pub(crate) fn new(reference: Weak<RefImpl>, name: &str) -> Arc<Self> {
        Arc::new_cyclic(|me| Self { reference, name: name.to_string(), state: Mutex::default(), me: me.clone() })
    }

    fn reference(&self) -> ChordResult<Arc<RefImpl>> {
        self.reference.upgrade().ok_or_else(|| ChordError::code_only(ChordErrorCode::Released, "reference released"))
    }

    /// The source the current implementation exposes under this name, if in the variant.
    fn current(&self) -> Option<ReadableRef> {
        let reference = self.reference.upgrade()?;
        let view = reference.current_view()?;
        if !reference.in_variant(&view, &self.name) {
            return None;
        }
        match view.get(&self.name) {
            Some(Member::State(readable)) => Some(readable.clone()),
            _ => None,
        }
    }

    /// The current source if its value can be read now (live, in the variant, hydrated).
    fn readable(&self) -> Option<ReadableRef> {
        self.current().filter(|source| source.hydrated())
    }

    fn report(&self, error: ChordError) {
        if let Some(reference) = self.reference.upgrade() {
            reference.object().host().report(error);
        }
    }

    fn safe(&self, listener: impl FnOnce()) {
        if let Err(panic) = catch_unwind(AssertUnwindSafe(listener)) {
            self.report(panic_error(panic.as_ref()));
        }
    }

    fn unsubscriber(&self, id: u64) -> Unsubscribe {
        let me = self.me.clone();
        Unsubscribe::new(move || {
            if let Some(me) = me.upgrade() {
                {
                    let mut state = lock(&me.state);
                    state.unseen.remove(&id);
                    state.values.shift_remove(&id);
                    state.events.shift_remove(&id);
                }
                me.bind();
            }
        })
    }

    /// Subscribes upstream to the current source again when it changed; a changed source is announced as a reset.
    pub(crate) fn bind(&self) {
        let listening = {
            let state = lock(&self.state);
            !state.values.is_empty() || !state.events.is_empty()
        };
        let source = if listening { self.current() } else { None };
        let (previous, generation) = {
            let mut state = lock(&self.state);
            if state.source.as_ref().map(readable_id) == source.as_ref().map(readable_id) {
                return;
            }
            state.source = source.clone();
            state.generation += 1;
            (state.upstream.take(), state.generation)
        };
        if let Some(previous) = previous {
            previous.unsubscribe();
        }
        let Some(source) = source else { return };
        let me = self.me.clone();
        let first = AtomicBool::new(true);
        let upstream = source.events(Arc::new(move |event: &StateEvent| {
            if let Some(me) = me.upgrade() {
                me.forward(event, generation, first.swap(false, Ordering::SeqCst));
            }
        }));
        let mut state = lock(&self.state);
        if state.generation == generation {
            state.upstream = Some(upstream);
        } else {
            drop(state);
            upstream.unsubscribe();
        }
    }

    fn forward(&self, event: &StateEvent, generation: u64, first: bool) {
        let mut state = lock(&self.state);
        if state.generation != generation {
            return;
        }
        let value = event.value().clone();
        let known = state.seen.as_ref().is_some_and(|seen| seen.same_ref(&value));
        if first && state.unseen.is_empty() && known {
            return;
        }
        state.seen = Some(value.clone());
        let translated = if first { StateEvent::Reset { value: value.clone() } } else { event.clone() };
        let events: Vec<(u64, EventListener)> = state.events.iter().map(|(id, l)| (*id, l.clone())).collect();
        let values: Vec<(u64, ValueListener)> = state.values.iter().map(|(id, l)| (*id, l.clone())).collect();
        let unseen = std::mem::take(&mut state.unseen);
        drop(state);
        let me = self.me.clone();
        // Queued: a rebind during a commit must not run observers before the commit completes.
        enqueue(move || {
            let Some(me) = me.upgrade() else { return };
            for (id, listener) in events {
                if !lock(&me.state).events.contains_key(&id) {
                    continue;
                }
                let snap = StateEvent::Snap { value: value.clone() };
                let delivered = if unseen.contains(&id) { &snap } else { &translated };
                me.safe(|| listener(delivered));
            }
            for (id, listener) in values {
                if lock(&me.state).values.contains_key(&id) {
                    me.safe(|| listener(&value));
                }
            }
        });
    }

    /// The reference ended: drops every listener and the upstream subscription.
    pub(crate) fn stop(&self) {
        {
            let mut state = lock(&self.state);
            state.values.clear();
            state.events.clear();
            state.unseen.clear();
        }
        self.bind();
    }
}

impl Readable for MemberReadable {
    fn value(&self) -> ChordResult<Json> {
        let reference = self.reference()?;
        let status = reference.status();
        if status.ended() {
            return Err(reference.terminal_error());
        }
        let what = || not_ready(&format!("{}.{}", reference.address(), self.name));
        if status != RefStatus::Live {
            return Err(what());
        }
        let Some(view) = reference.current_view() else { return Err(what()) };
        let Some(source) = self.current() else {
            let variant = view.variant_name().unwrap_or("(local)").to_string();
            let message = format!("{}: {} is not a state member of variant {variant}", reference.address(), self.name);
            return Err(ChordError::code_only(ChordErrorCode::NotInVariant, message));
        };
        source.value()
    }

    fn subscribe(&self, listener: ValueListener) -> Unsubscribe {
        let id = {
            let mut state = lock(&self.state);
            state.next += 1;
            let id = state.next;
            state.values.insert(id, listener.clone());
            id
        };
        if let Some(source) = self.readable() {
            match source.value() {
                Ok(value) => {
                    lock(&self.state).seen = Some(value.clone());
                    self.safe(|| listener(&value));
                }
                Err(error) => self.report(error),
            }
        }
        self.bind();
        self.unsubscriber(id)
    }

    fn events(&self, listener: EventListener) -> Unsubscribe {
        let id = {
            let mut state = lock(&self.state);
            state.next += 1;
            let id = state.next;
            state.events.insert(id, listener.clone());
            id
        };
        match self.readable() {
            Some(source) => match source.value() {
                Ok(value) => {
                    lock(&self.state).seen = Some(value.clone());
                    self.safe(|| listener(&StateEvent::Snap { value }));
                }
                Err(error) => self.report(error),
            },
            None => {
                lock(&self.state).unseen.insert(id);
            }
        }
        self.bind();
        self.unsubscriber(id)
    }

    /// Not hydrated while the reference is not live, or while its source (a forwarded remote member) is not.
    fn hydrated(&self) -> bool {
        match self.reference.upgrade() {
            Some(reference) if reference.status() == RefStatus::Live => {
                self.current().is_none_or(|source| source.hydrated())
            }
            _ => false,
        }
    }

    fn when_hydrated(&self) -> BoxFuture<ChordResult<()>> {
        let me = self.me.clone();
        Box::pin(async move {
            let Some(reference) = me.upgrade().and_then(|me| me.reference.upgrade()) else { return Ok(()) };
            reference.ready_task().await?;
            let source = me.upgrade().and_then(|me| me.current());
            match source {
                Some(source) => source.when_hydrated().await,
                None => Ok(()),
            }
        })
    }
}
