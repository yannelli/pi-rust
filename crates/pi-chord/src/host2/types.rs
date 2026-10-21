//! Port of upstream `types.ts`: the options of `get`, state events, readables, reference status and shapes.
//! The call context (`Ctx`) is a concrete type in [`super::object`]. Every readable carries JSON values.

use std::sync::Arc;

use crate::context::AbortSignal;
use crate::delta::Op;
use crate::error::{ChordError, ChordResult};
use crate::json::Json;
use crate::sync::{BoxFuture, Unsubscribe};

/// Options of `get`.
#[derive(Clone, Debug, Default)]
pub struct GetOptions {
    /// Who the object is for. Defaults to the host's own identity (its `HostOptions` value).
    pub principal: Option<Json>,
    /// False: a method-only reference. It receives no state and is ready once its object is live.
    pub state: Option<bool>,
    /// `Host::get` only: gives up waiting for the reference (which is released then) when it aborts.
    pub signal: Option<AbortSignal>,
}

impl GetOptions {
    /// Options for a `get` on behalf of the given identity.
    pub fn principal(principal: Json) -> Self {
        Self { principal: Some(principal), ..Self::default() }
    }
}

/// One delta notification of a readable.
#[derive(Clone, Debug, PartialEq)]
pub enum StateEvent {
    Snap { value: Json },
    Ops { ops: Vec<Op>, value: Json },
    Reset { value: Json },
}

impl StateEvent {
    /// The `type` field: `snap`, `ops` or `reset`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Snap { .. } => "snap",
            Self::Ops { .. } => "ops",
            Self::Reset { .. } => "reset",
        }
    }

    /// The value after this event.
    pub fn value(&self) -> &Json {
        match self {
            Self::Snap { value } | Self::Ops { value, .. } | Self::Reset { value } => value,
        }
    }
}

pub type ValueListener = Arc<dyn Fn(&Json) + Send + Sync>;
pub type EventListener = Arc<dyn Fn(&StateEvent) + Send + Sync>;

/// A readable state source: a cell, a derivation, or a reference's state member. Values are immutable.
pub trait Readable: Send + Sync + 'static {
    fn value(&self) -> ChordResult<Json>;

    /// Called with the current value immediately, then with every new value.
    fn subscribe(&self, listener: ValueListener) -> Unsubscribe;

    /// Delta stream: a snapshot immediately, then ops, or a reset after a migration or rebind.
    fn events(&self, listener: EventListener) -> Unsubscribe;

    /// False while the source has no value yet (a remote member before its first snapshot).
    fn hydrated(&self) -> bool {
        true
    }

    /// Resolves once hydrated; fails if the source ends first.
    fn when_hydrated(&self) -> BoxFuture<ChordResult<()>> {
        Box::pin(std::future::ready(Ok(())))
    }

    /// The object-owned source behind this readable (a cell or a derivation), if it is one.
    #[doc(hidden)]
    fn source(&self) -> Option<&crate::host2::cell::SourceCore> {
        None
    }
}

pub type ReadableRef = Arc<dyn Readable>;

/// Identity of a readable: the address of its allocation.
pub(crate) fn readable_id(readable: &ReadableRef) -> usize {
    Arc::as_ptr(readable) as *const () as usize
}

/// Subscribes a closure without wrapping it in an `Arc` first.
pub fn on_value(readable: &dyn Readable, listener: impl Fn(&Json) + Send + Sync + 'static) -> Unsubscribe {
    readable.subscribe(Arc::new(listener))
}

/// Subscribes a closure to the delta stream without wrapping it in an `Arc` first.
pub fn on_event(readable: &dyn Readable, listener: impl Fn(&StateEvent) + Send + Sync + 'static) -> Unsubscribe {
    readable.events(Arc::new(listener))
}

/// Status of a reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefStatus {
    Loading,
    Live,
    Released,
    Gone,
}

impl RefStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Live => "live",
            Self::Released => "released",
            Self::Gone => "gone",
        }
    }

    /// Released or gone: the reference does nothing more.
    pub fn ended(self) -> bool {
        matches!(self, Self::Released | Self::Gone)
    }
}

/// What a reference reaches now: its variant, its member names and the provider contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    pub variant: Option<String>,
    pub members: Vec<String>,
    /// Fingerprint of the provider contract.
    pub contract: String,
}

pub type ShapeListener = Arc<dyn Fn(&Shape) + Send + Sync>;

/// Called after every status change, with the error when the reference is gone.
pub type StatusListener = Arc<dyn Fn(RefStatus, Option<&ChordError>) + Send + Sync>;
