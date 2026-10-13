//! Publication-only state backed by an external authoritative revision source.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::listener::{ErrorReporter, StateListener, unhandled_reporter};
use super::publisher::Publisher;
use super::{ReplicatedState, StateMember};
use crate::context::Context;
use crate::delta::Op;
use crate::error::{ChordError, ChordResult};
use crate::json::Json;
use crate::sync::{Unsubscribe, lock};

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// One immutable authoritative revision committed after an attachment snapshot.
#[derive(Clone, Debug)]
pub struct ReplicatedStateSourceFrame {
    /// Monotonic source cursor. The first frame after a snapshot is `snapshot.cursor + 1`.
    pub cursor: i64,
    /// The exact value produced by this commit.
    pub value: Json,
    /// The exact batch that produced `value` from the revision before it.
    pub ops: Vec<Op>,
    pub context: Context,
}

/// The fixed snapshot captured at the atomic attachment boundary.
#[derive(Clone, Debug)]
pub struct ReplicatedStateSourceSnapshot {
    pub value: Json,
    pub cursor: i64,
}

/// Receives source frames in commit order.
pub type SourceFrameListener = Box<dyn Fn(ReplicatedStateSourceFrame) + Send + Sync>;

/// One attachment to a [`ReplicatedStateSource`].
pub trait ReplicatedStateSourceAttachment: Send + Sync {
    fn snapshot(&self) -> ReplicatedStateSourceSnapshot;
    /// Install the sole listener and synchronously drain buffered frames in commit order. Single use;
    /// every later commit, including reentrant ones, must also be delivered in order until disposal.
    fn activate(&self, listener: SourceFrameListener) -> ChordResult<()>;
    /// Stop delivery and release source resources. Idempotent.
    fn dispose(&self) -> ChordResult<()>;
}

/// An authoritative immutable revision source. `attach` atomically captures a snapshot and buffers
/// every later commit with no overlap or gap.
pub trait ReplicatedStateSource: Send + Sync {
    fn attach(&self) -> ChordResult<Box<dyn ReplicatedStateSourceAttachment>>;
}

/// Options for [`attach_replicated_state_source`].
#[derive(Clone, Default)]
pub struct ReplicatedStateSourceOptions {
    /// Receives source-contract and listener failures without throwing them into the source.
    pub on_error: Option<Arc<dyn Fn(ChordError) + Send + Sync>>,
}

struct Inner {
    publisher: Arc<Publisher>,
    attachment: Box<dyn ReplicatedStateSourceAttachment>,
    report: ErrorReporter,
    cursor: Mutex<i64>,
    disposed: AtomicBool,
}

/// A synchronously hydrated, publication-only state backed by one source attachment.
#[derive(Clone)]
pub struct AttachedReplicatedState(Arc<Inner>);

fn assert_cursor(cursor: i64, kind: &str) -> ChordResult<()> {
    if cursor.abs() > MAX_SAFE_INTEGER {
        return Err(ChordError::type_error(format!("Replicated state source {kind} cursor must be a safe integer")));
    }
    Ok(())
}

impl Inner {
    fn receive(&self, frame: ReplicatedStateSourceFrame) {
        if self.disposed.load(Ordering::SeqCst) {
            return;
        }
        if let Err(error) = self.accept(frame) {
            self.fail(error);
        }
    }

    fn accept(&self, frame: ReplicatedStateSourceFrame) -> ChordResult<()> {
        assert_cursor(frame.cursor, "frame")?;
        {
            let mut cursor = lock(&self.cursor);
            let expected = *cursor + 1;
            if frame.cursor != expected {
                return Err(ChordError::error(format!(
                    "Replicated state source cursor has a gap: expected {expected}, received {}",
                    frame.cursor
                )));
            }
            *cursor = frame.cursor;
            self.publisher.enqueue(frame.value, frame.ops, frame.context);
        }
        if let Err(error) = ChordError::collect(self.publisher.deliver(), "Replicated state listeners failed") {
            (self.report)(error);
        }
        Ok(())
    }

    fn fail(&self, error: ChordError) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        match self.attachment.dispose() {
            Ok(()) => (self.report)(error),
            Err(dispose) => (self.report)(ChordError::Aggregate {
                message: "Replicated state source contract failed".into(),
                errors: vec![error, dispose],
            }),
        }
    }
}

impl AttachedReplicatedState {
    /// The last published value.
    pub fn value(&self) -> Json {
        self.0.publisher.value()
    }

    pub fn subscribe(&self, listener: StateListener) -> Unsubscribe {
        self.0.publisher.subscribe(listener)
    }

    /// Release the source attachment. The last published value stays readable.
    pub fn dispose(&self) -> ChordResult<()> {
        if self.0.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.0.attachment.dispose()
    }

    pub fn member(&self) -> StateMember {
        StateMember(self.0.publisher.clone())
    }
}

impl ReplicatedState for AttachedReplicatedState {
    fn value(&self) -> Option<Json> {
        Some(AttachedReplicatedState::value(self))
    }

    fn subscribe(&self, listener: StateListener) -> Unsubscribe {
        AttachedReplicatedState::subscribe(self, listener)
    }
}

impl From<&AttachedReplicatedState> for StateMember {
    fn from(state: &AttachedReplicatedState) -> Self {
        state.member()
    }
}

/// Attach a publication-only replicated state to one authoritative source stream.
pub fn attach_replicated_state_source(
    source: &dyn ReplicatedStateSource,
    options: ReplicatedStateSourceOptions,
) -> ChordResult<AttachedReplicatedState> {
    let attachment = source.attach()?;
    let snapshot = attachment.snapshot();
    if let Err(error) = assert_cursor(snapshot.cursor, "snapshot") {
        return Err(cleanup(error, attachment.dispose()));
    }
    let user_report = options.on_error.clone();
    let fallback = unhandled_reporter();
    let report: ErrorReporter = Arc::new(move |error| match &user_report {
        Some(handler) => handler(error),
        None => fallback(error),
    });
    let publisher = Publisher::new(snapshot.value, report.clone());
    let inner = Arc::new(Inner {
        publisher,
        attachment,
        report,
        cursor: Mutex::new(snapshot.cursor),
        disposed: AtomicBool::new(false),
    });
    // The listener keeps the state alive until the source drops it on disposal, as upstream does.
    let receiver = inner.clone();
    let activated = inner.attachment.activate(Box::new(move |frame| receiver.receive(frame)));
    if let Err(error) = activated {
        return Err(cleanup(error, inner.attachment.dispose()));
    }
    Ok(AttachedReplicatedState(inner))
}

fn cleanup(error: ChordError, dispose: ChordResult<()>) -> ChordError {
    match dispose {
        Ok(()) => error,
        Err(dispose) => ChordError::Aggregate {
            message: "Failed to attach replicated state source".into(),
            errors: vec![error, dispose],
        },
    }
}
