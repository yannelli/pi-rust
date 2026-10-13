//! Local publication order for one authoritative state, independent of how revisions are produced.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::listener::{DeliveryKind, ErrorReporter, Frame, ReplicatedStateDelivery, StateListener, StateSubscriber};
use crate::context::{BACKGROUND_CONTEXT, Context};
use crate::delta::Op;
use crate::error::{ChordError, ChordResult};
use crate::json::Json;
use crate::sync::{Unsubscribe, lock};

/// Receives every exact operation batch synchronously, in publication order.
pub(crate) type SourceListener = Arc<dyn Fn(&[Op], u64, &Context) -> ChordResult<()> + Send + Sync>;

struct Publication {
    value: Json,
    ops: Arc<Vec<Op>>,
    sequence: u64,
    context: Context,
}

struct State {
    value: Json,
    sequence: u64,
    listeners: Vec<(u64, Arc<StateSubscriber>, u64)>,
    sources: Vec<(u64, SourceListener)>,
    publications: VecDeque<Publication>,
    delivering: bool,
    next_id: u64,
}

pub(crate) struct Publisher {
    state: Mutex<State>,
    report_error: ErrorReporter,
}

/// The context for synthetic deliveries that have no caller.
pub(crate) fn service_delivery_context() -> Context {
    BACKGROUND_CONTEXT.clone()
}

impl Publisher {
    pub fn new(initial: Json, report_error: ErrorReporter) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                value: initial,
                sequence: 0,
                listeners: Vec::new(),
                sources: Vec::new(),
                publications: VecDeque::new(),
                delivering: false,
                next_id: 0,
            }),
            report_error,
        })
    }

    pub fn value(&self) -> Json {
        lock(&self.state).value.clone()
    }

    /// Atomically capture the value and its matching publication sequence.
    pub fn snapshot(&self) -> (Json, u64) {
        let state = lock(&self.state);
        (state.value.clone(), state.sequence)
    }

    pub fn subscribe(self: &Arc<Self>, listener: StateListener) -> Unsubscribe {
        let subscriber = StateSubscriber::new(listener, self.report_error.clone());
        let (id, value, sequence) = {
            let mut state = lock(&self.state);
            state.next_id += 1;
            let id = state.next_id;
            let sequence = state.sequence;
            state.listeners.push((id, subscriber.clone(), sequence));
            (id, state.value.clone(), sequence)
        };
        let delivery = ReplicatedStateDelivery { kind: DeliveryKind::Hydrate, sequence };
        subscriber.push(Frame { value, context: service_delivery_context(), delivery });
        subscriber.drain();
        let weak = Arc::downgrade(self);
        Unsubscribe::new(move || {
            subscriber.close();
            if let Some(publisher) = weak.upgrade() {
                lock(&publisher.state).listeners.retain(|(existing, ..)| *existing != id);
            }
        })
    }

    pub fn subscribe_source(self: &Arc<Self>, listener: SourceListener) -> Unsubscribe {
        let id = {
            let mut state = lock(&self.state);
            state.next_id += 1;
            let id = state.next_id;
            state.sources.push((id, listener));
            id
        };
        let weak = Arc::downgrade(self);
        Unsubscribe::new(move || {
            if let Some(publisher) = weak.upgrade() {
                lock(&publisher.state).sources.retain(|(existing, _)| *existing != id);
            }
        })
    }

    /// Record a prepared revision; its sequence is fixed here, in commit order.
    pub fn enqueue(&self, value: Json, ops: Vec<Op>, context: Context) {
        let mut state = lock(&self.state);
        state.value = value.clone();
        state.sequence += 1;
        let sequence = state.sequence;
        state.publications.push_back(Publication { value, ops: Arc::new(ops), sequence, context });
    }

    /// Deliver queued publications and return isolated source-listener failures. A reentrant or
    /// concurrent call returns at once; the active delivery drains its publications too.
    pub fn deliver(&self) -> Vec<ChordError> {
        {
            let mut state = lock(&self.state);
            if state.delivering {
                return Vec::new();
            }
            state.delivering = true;
        }
        let mut errors = Vec::new();
        loop {
            let (publication, sources, listeners) = {
                let mut state = lock(&self.state);
                match state.publications.pop_front() {
                    Some(publication) => {
                        let sources: Vec<SourceListener> = state.sources.iter().map(|(_, l)| l.clone()).collect();
                        let listeners: Vec<(Arc<StateSubscriber>, u64)> =
                            state.listeners.iter().map(|(_, s, hydrated)| (s.clone(), *hydrated)).collect();
                        (publication, sources, listeners)
                    }
                    None => {
                        state.delivering = false;
                        return errors;
                    }
                }
            };
            for source in sources {
                if let Err(error) = source(&publication.ops, publication.sequence, &publication.context) {
                    errors.push(error);
                }
            }
            let delivery = ReplicatedStateDelivery { kind: DeliveryKind::Update, sequence: publication.sequence };
            for (subscriber, hydrated) in listeners {
                if publication.sequence <= hydrated {
                    continue;
                }
                subscriber.push(Frame {
                    value: publication.value.clone(),
                    context: publication.context.clone(),
                    delivery,
                });
                subscriber.drain();
            }
        }
    }
}
