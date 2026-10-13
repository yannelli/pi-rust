//! Authoritative mutable replicated state: atomic copy-on-write changes and whole-root replacement.

use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use super::listener::{StateListener, unhandled_reporter};
use super::publisher::Publisher;
use super::{ReplicatedState, StateMember};
use crate::context::Context;
use crate::delta::{Tracker, track};
use crate::error::{ChordError, ChordResult};
use crate::json::Json;
use crate::sync::{Unsubscribe, lock};

struct Inner {
    tracker: Mutex<Tracker>,
    publisher: Arc<Publisher>,
    commit: Mutex<()>,
    changing: Mutex<Option<ThreadId>>,
}

/// Authoritative state that always has a value. Clones share the same state.
#[derive(Clone)]
pub struct MutableReplicatedState(Arc<Inner>);

/// Clears the "inside a change callback" marker even when the callback fails.
struct ChangingGuard<'a>(&'a Mutex<Option<ThreadId>>);

impl Drop for ChangingGuard<'_> {
    fn drop(&mut self) {
        *lock(self.0) = None;
    }
}

impl MutableReplicatedState {
    /// Take immutable ownership of the starting root.
    pub fn new(root: Json) -> Self {
        let tracker = track(root);
        let publisher = Publisher::new(tracker.value().clone(), unhandled_reporter());
        Self(Arc::new(Inner {
            tracker: Mutex::new(tracker),
            publisher,
            commit: Mutex::new(()),
            changing: Mutex::new(None),
        }))
    }

    /// The latest committed value.
    pub fn value(&self) -> Json {
        lock(&self.0.tracker).value().clone()
    }

    fn reentrant(&self) -> bool {
        *lock(&self.0.changing) == Some(std::thread::current().id())
    }

    /// Atomically publish one synchronous mutation of a copy-on-write draft.
    ///
    /// When `mutate` fails, the value and sequence stay unchanged. A deeply equal result publishes nothing.
    pub fn change(&self, context: &Context, mutate: impl FnOnce(&mut Json) -> ChordResult<()>) -> ChordResult<()> {
        if self.reentrant() {
            return Err(ChordError::error("Replicated state cannot be changed reentrantly from a change callback"));
        }
        {
            let _commit = lock(&self.0.commit);
            let mut change = lock(&self.0.tracker).begin_change();
            let prepared = {
                *lock(&self.0.changing) = Some(std::thread::current().id());
                let _guard = ChangingGuard(&self.0.changing);
                let outcome = change.state().and_then(mutate).and_then(|()| change.prepare());
                match outcome {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        change.abort();
                        return Err(error);
                    }
                }
            };
            lock(&self.0.tracker).adopt(&prepared)?;
            if prepared.ops.is_empty() {
                return Ok(());
            }
            self.0.publisher.enqueue(prepared.value, prepared.ops, context.clone());
        }
        ChordError::collect(self.0.publisher.deliver(), "Replicated state listeners failed")
    }

    /// Atomically take immutable ownership of a replacement root.
    pub fn replace(&self, context: &Context, value: Json) -> ChordResult<()> {
        if self.reentrant() {
            return Err(ChordError::error("Replicated state cannot be replaced from a change callback"));
        }
        {
            let _commit = lock(&self.0.commit);
            let mut tracker = lock(&self.0.tracker);
            let prepared = tracker.prepare_replace(value);
            tracker.adopt(&prepared)?;
            drop(tracker);
            if prepared.ops.is_empty() {
                return Ok(());
            }
            self.0.publisher.enqueue(prepared.value, prepared.ops, context.clone());
        }
        ChordError::collect(self.0.publisher.deliver(), "Replicated state listeners failed")
    }

    /// Serialize callbacks for one subscription, starting with the current value.
    pub fn subscribe(&self, listener: StateListener) -> Unsubscribe {
        self.0.publisher.subscribe(listener)
    }

    /// The publication handle a service implementation exposes as a state member.
    pub fn member(&self) -> StateMember {
        StateMember(self.0.publisher.clone())
    }
}

impl ReplicatedState for MutableReplicatedState {
    fn value(&self) -> Option<Json> {
        Some(MutableReplicatedState::value(self))
    }

    fn subscribe(&self, listener: StateListener) -> Unsubscribe {
        MutableReplicatedState::subscribe(self, listener)
    }
}

impl From<&MutableReplicatedState> for StateMember {
    fn from(state: &MutableReplicatedState) -> Self {
        state.member()
    }
}

/// Create authoritative state by taking immutable ownership of `root`.
pub fn replicated_state(root: Json) -> MutableReplicatedState {
    MutableReplicatedState::new(root)
}
