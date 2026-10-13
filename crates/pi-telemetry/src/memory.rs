//! Backend-neutral reference implementation that records spans in process memory.

use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::attributes::SpanAttributes;
use crate::noop::NoopTelemetryContext;
use crate::span::{SpanOptions, SpanOutcome, SpanStatus, TelemetryContext, TelemetrySpan};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedTelemetryEvent {
    pub name: String,
    pub attributes: SpanAttributes,
}

/// A detached snapshot of one recorded span.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedTelemetrySpan {
    pub id: u64,
    pub parent_id: Option<u64>,
    pub name: String,
    pub attributes: SpanAttributes,
    pub events: Vec<RecordedTelemetryEvent>,
    pub status: SpanStatus,
    pub settled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_sequence: Option<u64>,
}

struct Recorded {
    span: RecordedTelemetrySpan,
    explicit_status: bool,
}

struct State {
    spans: Vec<Recorded>,
    next_span_id: u64,
    next_end_sequence: u64,
}

type Shared = Arc<Mutex<State>>;

fn lock(state: &Shared) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records spans in start order. Create a fresh instance to isolate tests or recording scopes.
#[derive(Clone)]
pub struct InMemoryTelemetryContext {
    state: Shared,
}

impl Default for InMemoryTelemetryContext {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryTelemetryContext {
    pub fn new() -> Self {
        Self { state: Arc::new(Mutex::new(State { spans: Vec::new(), next_span_id: 1, next_end_sequence: 1 })) }
    }

    /// Detached snapshots in span-start order.
    pub fn get_spans(&self) -> Vec<RecordedTelemetrySpan> {
        lock(&self.state).spans.iter().map(|recorded| recorded.span.clone()).collect()
    }
}

fn open(state: &Shared, parent: Option<usize>, options: SpanOptions) -> Arc<dyn TelemetrySpan> {
    let mut guard = lock(state);
    let parent_id = match parent {
        Some(index) if guard.spans[index].span.settled => return Arc::new(NoopTelemetryContext),
        Some(index) => Some(guard.spans[index].span.id),
        None => None,
    };
    let id = guard.next_span_id;
    guard.next_span_id += 1;
    let span = RecordedTelemetrySpan {
        id,
        parent_id,
        name: options.name,
        attributes: options.attributes,
        events: Vec::new(),
        status: SpanStatus::Ok,
        settled: false,
        end_sequence: None,
    };
    guard.spans.push(Recorded { span, explicit_status: false });
    Arc::new(MemorySpan { state: state.clone(), index: guard.spans.len() - 1 })
}

impl TelemetryContext for InMemoryTelemetryContext {
    fn open_span(&self, options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        open(&self.state, None, options)
    }
}

struct MemorySpan {
    state: Shared,
    index: usize,
}

impl MemorySpan {
    /// Apply `update` while the span is open; calls after settlement are inert.
    fn update(&self, update: impl FnOnce(&mut Recorded)) {
        let mut guard = lock(&self.state);
        let recorded = &mut guard.spans[self.index];
        if !recorded.span.settled {
            update(recorded);
        }
    }
}

impl TelemetryContext for MemorySpan {
    fn open_span(&self, options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        open(&self.state, Some(self.index), options)
    }
}

impl TelemetrySpan for MemorySpan {
    fn add_event(&self, name: &str, attributes: SpanAttributes) {
        self.update(|recorded| recorded.span.events.push(RecordedTelemetryEvent { name: name.into(), attributes }));
    }

    fn set_attributes(&self, attributes: SpanAttributes) {
        self.update(|recorded| recorded.span.attributes.merge(&attributes));
    }

    fn set_status(&self, status: SpanStatus) {
        self.update(|recorded| {
            recorded.span.status = status;
            recorded.explicit_status = true;
        });
    }

    fn end(&self, outcome: SpanOutcome) {
        let mut guard = lock(&self.state);
        let sequence = guard.next_end_sequence;
        let recorded = &mut guard.spans[self.index];
        if recorded.span.settled {
            return;
        }
        if let SpanOutcome::Failed(error) = outcome
            && !recorded.explicit_status
        {
            recorded.span.status = SpanStatus::Error { error };
        }
        recorded.span.settled = true;
        recorded.span.end_sequence = Some(sequence);
        guard.next_end_sequence += 1;
    }
}
