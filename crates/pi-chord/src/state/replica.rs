//! A cold read-only replica used by service consumers until a complete snapshot arrives.

use std::sync::{Arc, Mutex};

use super::ReplicatedState;
use super::listener::{DeliveryKind, ErrorReporter, Frame, ReplicatedStateDelivery, StateListener, StateSubscriber};
use super::publisher::service_delivery_context;
use crate::context::Context;
use crate::delta::{Op, apply_immutable, is_base};
use crate::error::{ChordError, ChordResult};
use crate::json::Json;
use crate::sync::{Unsubscribe, lock};

#[derive(Default)]
struct State {
    current: Option<(Json, u64)>,
    listeners: Vec<(u64, Arc<StateSubscriber>)>,
    next_id: u64,
}

/// A replica that is unready (`None`) until hydrated and after it is cleared.
pub struct ReplicatedStateReplica {
    state: Mutex<State>,
    report_error: ErrorReporter,
}

impl ReplicatedStateReplica {
    /// A cold replica; listener failures go to `report_error`.
    pub fn new(report_error: ErrorReporter) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(State::default()), report_error })
    }

    pub fn value(&self) -> Option<Json> {
        lock(&self.state).current.as_ref().map(|(value, _)| value.clone())
    }

    /// The sequence of the installed value, when hydrated.
    pub fn sequence(&self) -> Option<u64> {
        lock(&self.state).current.as_ref().map(|(_, sequence)| *sequence)
    }

    pub fn subscribe(self: &Arc<Self>, listener: StateListener) -> Unsubscribe {
        let subscriber = StateSubscriber::new(listener, self.report_error.clone());
        let (id, current) = {
            let mut state = lock(&self.state);
            state.next_id += 1;
            let id = state.next_id;
            state.listeners.push((id, subscriber.clone()));
            (id, state.current.clone())
        };
        if let Some((value, sequence)) = current {
            let delivery = ReplicatedStateDelivery { kind: DeliveryKind::Hydrate, sequence };
            subscriber.push(Frame { value, context: service_delivery_context(), delivery });
            subscriber.drain();
        }
        let weak = Arc::downgrade(self);
        Unsubscribe::new(move || {
            subscriber.close();
            if let Some(replica) = weak.upgrade() {
                lock(&replica.state).listeners.retain(|(existing, _)| *existing != id);
            }
        })
    }

    /// Install a complete base batch at `sequence`.
    pub fn hydrate(&self, sequence: u64, ops: &[Op], context: &Context) -> ChordResult<()> {
        let next = if is_base(ops) {
            apply_immutable(None, ops)
        } else {
            Err(ChordError::error("Replicated state snapshot is not a base operation batch"))
        };
        let next = next.inspect_err(|_| self.clear())?;
        self.install(next, sequence, context, DeliveryKind::Hydrate);
        Ok(())
    }

    /// Apply the batch that must directly follow the installed sequence.
    pub fn update(&self, sequence: u64, ops: &[Op], context: &Context) -> ChordResult<()> {
        let Some((value, current)) = lock(&self.state).current.clone() else {
            return Err(ChordError::error("Replicated state received an update before hydration"));
        };
        if sequence != current + 1 {
            self.clear();
            return Err(ChordError::error("Replicated state update sequence has a gap"));
        }
        let next = apply_immutable(Some(&value), ops).inspect_err(|_| self.clear())?;
        self.install(next, sequence, context, DeliveryKind::Update);
        Ok(())
    }

    /// Become unready and discard pending deliveries.
    pub fn clear(&self) {
        let listeners = {
            let mut state = lock(&self.state);
            state.current = None;
            state.listeners.iter().map(|(_, subscriber)| subscriber.clone()).collect::<Vec<_>>()
        };
        for subscriber in listeners {
            subscriber.clear();
        }
    }

    fn install(&self, value: Json, sequence: u64, context: &Context, kind: DeliveryKind) {
        let subscribers = {
            let mut state = lock(&self.state);
            state.current = Some((value.clone(), sequence));
            state.listeners.iter().map(|(_, subscriber)| subscriber.clone()).collect::<Vec<_>>()
        };
        let frame = Frame { value, context: context.clone(), delivery: ReplicatedStateDelivery { kind, sequence } };
        // Enqueue for everyone before user code can publish another revision reentrantly.
        for subscriber in &subscribers {
            subscriber.push(frame.clone());
        }
        for subscriber in &subscribers {
            subscriber.drain();
        }
    }
}

impl ReplicatedState for Arc<ReplicatedStateReplica> {
    fn value(&self) -> Option<Json> {
        ReplicatedStateReplica::value(self)
    }

    fn subscribe(&self, listener: StateListener) -> Unsubscribe {
        ReplicatedStateReplica::subscribe(self, listener)
    }
}
