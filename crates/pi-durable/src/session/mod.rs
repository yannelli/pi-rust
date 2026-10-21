//! The Session kernel (spec sections 3, 4, 9.1, 9.2, and 10.1): one mutation line, transactions over tables and
//! Chord-tracked documents, the loaded tracker cache, committed publication, states, and watches.

mod assemble;
mod cache;
mod core;
mod draft;
mod forks;
mod guarded;
mod line;
mod observation;
mod observe;
mod plans;
mod reads;
mod tx;
mod tx_docs;
mod tx_writes;
mod watch;

use std::sync::Arc;

use async_trait::async_trait;
use pi_chord::Context;

pub use self::core::{OnLineDocument, Session};
pub use cache::{DEFAULT_DOCUMENT_RETENTION_MS, DocumentCacheSizes, document_retention};
pub use draft::{Draft, DraftCore};
pub use forks::{ForkDocumentCopy, prepare_fork_document_copies};
pub use line::SharedResult;
pub use observation::{CommittedObserver, CommittedStateSource, Release, retirement_operations};
pub use tx::{TransactionScope, Tx, TxFuture};
pub use watch::{CommittedWatch, DocumentWatch, MAX_PENDING_WATCH_FRAMES, ReplaceValue};

use crate::errors::{DurableError, DurableResult};
use crate::types::{CommitPublication, ConversationRecord, ReportInfo, Storage};

/// A document state: hydrated, read-only, and bound to one committed incarnation. Retirement publishes `null`.
pub type DocumentState = pi_chord::state::AttachedReplicatedState;

/// How the Session ended: `closed`, or `failed` with the error that failed it.
#[derive(Clone, Debug)]
pub enum SessionEnd {
    Closed,
    Failed(DurableError),
}

impl SessionEnd {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Failed(_) => "failed",
        }
    }
}

/// Why a watch ended (spec section 9.1).
#[derive(Clone, Debug)]
pub enum WatchEnd {
    Stopped,
    Cancelled,
    SessionClosed,
    Retired,
    ListenerError(DurableError),
    /// The Session failed; the error is what failed it, usually a storage error.
    SessionFailed(DurableError),
}

impl WatchEnd {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Cancelled => "cancelled",
            Self::SessionClosed => "session_closed",
            Self::Retired => "retired",
            Self::ListenerError(_) => "listener_error",
            Self::SessionFailed(_) => "session_failed",
        }
    }

    pub fn error(&self) -> Option<&DurableError> {
        match self {
            Self::ListenerError(error) | Self::SessionFailed(error) => Some(error),
            _ => None,
        }
    }
}

/// Synchronous post-adoption commit listener. It must not block or call Session operations.
pub type CommitListener = Arc<dyn Fn(&CommitPublication, &Context) -> DurableResult<()> + Send + Sync>;

/// Extension points a layer above the Session (the Harness) supplies; upstream subclasses `SessionImpl`.
#[async_trait]
pub trait SessionHooks: Send + Sync {
    /// Runs inside every transaction that creates or forks a conversation, after its record is staged.
    async fn conversation_created(&self, _tx: &Tx, _record: &ConversationRecord) -> DurableResult<()> {
        Ok(())
    }

    /// Runs after close seals admission and before the line closes Storage.
    async fn before_close(&self) {}

    /// How long a loaded document that nothing observes stays cached after its last use, read at each use; `None`:
    /// the default. See [`document_retention`] for what a value means.
    fn document_retention_ms(&self) -> Option<f64> {
        None
    }

    /// The Session's failure, and errors of listeners, which never fail what ran them, with what each is about and,
    /// when known, the context of the operation that raised it.
    fn report(&self, _error: &DurableError, _info: &ReportInfo, _context: Option<&Context>) {}
}

struct NoHooks;

impl SessionHooks for NoHooks {}

/// Options of [`create_session`].
#[derive(Clone, Default)]
pub struct SessionOptions {
    /// Wall clock in milliseconds for task times; default the system clock.
    pub now: Option<Arc<dyn Fn() -> f64 + Send + Sync>>,
    pub hooks: Option<Arc<dyn SessionHooks>>,
    /// How long a loaded document nothing observes stays cached after its last use: default ten minutes, `0` drops it
    /// right after use, `f64::INFINITY` keeps documents; anything else that is not a count of milliseconds means the
    /// default.
    pub document_retention_ms: Option<f64>,
}

/// Open a Session kernel over one storage backend. The first error a Storage method returns fails the Session.
pub fn create_session(storage: Arc<dyn Storage>, options: SessionOptions) -> Session {
    let hooks = options.hooks.unwrap_or_else(|| Arc::new(NoHooks));
    let now = options.now.unwrap_or_else(|| Arc::new(system_now));
    let (given, source) = (options.document_retention_ms, hooks.clone());
    let retention = Arc::new(move || given.or_else(|| source.document_retention_ms()));
    Session::open(storage, hooks, now, retention)
}

fn system_now() -> f64 {
    let elapsed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    elapsed.as_millis() as f64
}
