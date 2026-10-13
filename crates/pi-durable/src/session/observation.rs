//! Session-to-Chord bridge for document states and the observer contract shared with watches.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};

use pi_chord::delta::Op;
use pi_chord::state::{
    ReplicatedStateSource, ReplicatedStateSourceAttachment, ReplicatedStateSourceFrame, ReplicatedStateSourceSnapshot,
    SourceFrameListener,
};
use pi_chord::{ChordError, ChordResult, Context, Json};

use super::line::lock;
use crate::errors::DurableError;

/// Canonical terminal update for a retired document incarnation.
pub fn retirement_operations() -> Vec<Op> {
    vec![Op::Replace(Json::Null)]
}

/// A committed observer bound to one incarnation or view: a JSON `null` value retires it.
pub trait CommittedObserver: Send + Sync {
    fn advance(&self, value: Json, ops: Vec<Op>, context: Context);
    /// End with the Session: closed, or failed with its first error.
    fn close_session(&self, failure: Option<DurableError>);
    /// Whether frames carry the commit's cancellation; a document state's frames carry none.
    fn keeps_cancellation(&self) -> bool;
}

type Cleanup = Box<dyn FnOnce() + Send>;

/// Releases the Session subscriptions of one observer. Filled after the observer exists; idempotent.
#[derive(Default)]
pub struct Release {
    slot: Mutex<(bool, Vec<Cleanup>)>,
}

impl Release {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Add a cleanup action; it runs at once when the release already ran.
    pub fn push(&self, action: impl FnOnce() + Send + 'static) {
        let mut slot = lock(&self.slot);
        if slot.0 {
            drop(slot);
            action();
            return;
        }
        slot.1.push(Box::new(action));
    }

    pub fn run(&self) {
        let actions = {
            let mut slot = lock(&self.slot);
            slot.0 = true;
            std::mem::take(&mut slot.1)
        };
        for action in actions {
            action();
        }
    }
}

struct SourceState {
    attachments: Vec<Arc<SourceAttachment>>,
    value: Option<Json>,
    cursor: i64,
    retired: bool,
    closed: bool,
}

/// Session-to-Chord bridge owned one-to-one by one attached state. A `null` value retires it.
pub struct CommittedStateSource {
    state: Mutex<SourceState>,
    release: Arc<Release>,
    weak: Weak<Self>,
}

impl CommittedStateSource {
    pub fn new(value: Json, release: Arc<Release>) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            state: Mutex::new(SourceState {
                attachments: Vec::new(),
                value: Some(value),
                cursor: 0,
                retired: false,
                closed: false,
            }),
            release,
            weak: weak.clone(),
        })
    }

    fn finish_disposal(&self) {
        {
            let mut state = lock(&self.state);
            if state.closed {
                return;
            }
            state.closed = true;
            state.value = None;
        }
        self.release.run();
    }

    fn detach(&self, attachment: &Arc<SourceAttachment>) {
        let empty = {
            let mut state = lock(&self.state);
            state.attachments.retain(|other| !Arc::ptr_eq(other, attachment));
            state.attachments.is_empty()
        };
        if empty {
            self.finish_disposal();
        }
    }
}

impl CommittedObserver for CommittedStateSource {
    fn advance(&self, value: Json, ops: Vec<Op>, context: Context) {
        let (frame, attachments) = {
            let mut state = lock(&self.state);
            if state.closed || state.retired {
                return;
            }
            state.cursor += 1;
            state.retired = value.is_null();
            state.value = Some(value.clone());
            let frame = ReplicatedStateSourceFrame { cursor: state.cursor, value, ops, context };
            (frame, state.attachments.clone())
        };
        for attachment in attachments {
            attachment.publish(frame.clone());
        }
    }

    /// Stop publishing; the attached states keep their last value.
    fn close_session(&self, _failure: Option<DurableError>) {
        let attachments = {
            let state = lock(&self.state);
            if state.closed {
                return;
            }
            state.attachments.clone()
        };
        for attachment in attachments {
            attachment.dispose_inner();
        }
        self.finish_disposal();
    }

    fn keeps_cancellation(&self) -> bool {
        false
    }
}

impl ReplicatedStateSource for CommittedStateSource {
    fn attach(&self) -> ChordResult<Box<dyn ReplicatedStateSourceAttachment>> {
        let mut state = lock(&self.state);
        if state.closed {
            return Err(ChordError::error("State source is closed"));
        }
        let snapshot =
            ReplicatedStateSourceSnapshot { value: state.value.clone().unwrap_or(Json::Null), cursor: state.cursor };
        let attachment = Arc::new(SourceAttachment {
            snapshot,
            source: self.weak.clone(),
            state: Mutex::new(AttachmentState::default()),
        });
        state.attachments.push(attachment.clone());
        Ok(Box::new(AttachmentHandle(attachment)))
    }
}

#[derive(Default)]
struct AttachmentState {
    frames: VecDeque<ReplicatedStateSourceFrame>,
    listener: Option<Arc<SourceFrameListener>>,
    activated: bool,
    disposed: bool,
    scheduled: bool,
    delivering: bool,
}

struct SourceAttachment {
    snapshot: ReplicatedStateSourceSnapshot,
    source: Weak<CommittedStateSource>,
    state: Mutex<AttachmentState>,
}

impl SourceAttachment {
    fn publish(self: &Arc<Self>, frame: ReplicatedStateSourceFrame) {
        let mut state = lock(&self.state);
        if state.disposed {
            return;
        }
        state.frames.push_back(frame);
        if !state.activated || state.delivering || state.scheduled {
            return;
        }
        state.scheduled = true;
        drop(state);
        let attachment = self.clone();
        tokio::spawn(async move {
            {
                let mut state = lock(&attachment.state);
                state.scheduled = false;
                if state.disposed {
                    return;
                }
            }
            attachment.drain();
        });
    }

    fn drain(&self) {
        let listener = {
            let mut state = lock(&self.state);
            if state.delivering || state.disposed {
                return;
            }
            let Some(listener) = state.listener.clone() else { return };
            state.delivering = true;
            listener
        };
        loop {
            let frame = {
                let mut state = lock(&self.state);
                if state.disposed {
                    state.delivering = false;
                    return;
                }
                match state.frames.pop_front() {
                    Some(frame) => frame,
                    None => {
                        state.delivering = false;
                        return;
                    }
                }
            };
            listener(frame);
        }
    }

    /// Dispose without notifying the source; returns whether this call disposed it.
    fn dispose_inner(&self) -> bool {
        let mut state = lock(&self.state);
        if state.disposed {
            return false;
        }
        state.disposed = true;
        state.frames.clear();
        state.listener = None;
        true
    }
}

struct AttachmentHandle(Arc<SourceAttachment>);

impl ReplicatedStateSourceAttachment for AttachmentHandle {
    fn snapshot(&self) -> ReplicatedStateSourceSnapshot {
        self.0.snapshot.clone()
    }

    fn activate(&self, listener: SourceFrameListener) -> ChordResult<()> {
        {
            let mut state = lock(&self.0.state);
            if state.activated {
                return Err(ChordError::error("State attachment is already active"));
            }
            if state.disposed {
                return Err(ChordError::error("State attachment is disposed"));
            }
            state.activated = true;
            state.listener = Some(Arc::new(listener));
        }
        self.0.drain();
        Ok(())
    }

    fn dispose(&self) -> ChordResult<()> {
        if self.0.dispose_inner()
            && let Some(source) = self.0.source.upgrade()
        {
            source.detach(&self.0);
        }
        Ok(())
    }
}
