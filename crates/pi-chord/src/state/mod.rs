//! Replicated state: authoritative one-writer latest-value state with local and remote replicas.

mod attached;
mod listener;
mod mutable;
mod publisher;
mod replica;

use std::sync::Arc;

pub use attached::{
    AttachedReplicatedState, ReplicatedStateSource, ReplicatedStateSourceAttachment, ReplicatedStateSourceFrame,
    ReplicatedStateSourceOptions, ReplicatedStateSourceSnapshot, SourceFrameListener, attach_replicated_state_source,
};
pub use listener::{DeliveryKind, ErrorReporter, ReplicatedStateDelivery, StateListener};
pub use mutable::{MutableReplicatedState, replicated_state};
pub(crate) use publisher::{Publisher, service_delivery_context};
pub use replica::ReplicatedStateReplica;

use crate::context::Context;
use crate::delta::Op;
use crate::error::ChordResult;
use crate::json::Json;
use crate::sync::Unsubscribe;

/// Read access shared by authoritative states and replicas.
pub trait ReplicatedState: Send + Sync {
    /// The immutable value, or `None` until a replica is hydrated.
    fn value(&self) -> Option<Json>;
    /// Serialize callbacks for one subscription. At most 100 deliveries wait behind the running
    /// callback; overflow keeps only the newest, so update sequences may skip.
    fn subscribe(&self, listener: StateListener) -> Unsubscribe;
}

/// The publication side of a state, as a remote service implementation exposes it.
#[derive(Clone)]
pub struct StateMember(pub(crate) Arc<Publisher>);

impl StateMember {
    /// Atomically capture the value and its publication sequence.
    pub fn snapshot(&self) -> (Json, u64) {
        self.0.snapshot()
    }

    /// Receive every exact operation batch synchronously, in publication order, with its sequence.
    pub fn subscribe_ops(
        &self,
        listener: impl Fn(&[Op], u64, &Context) -> ChordResult<()> + Send + Sync + 'static,
    ) -> Unsubscribe {
        self.0.subscribe_source(Arc::new(listener))
    }
}
