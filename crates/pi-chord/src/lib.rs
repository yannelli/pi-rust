//! Chord: an application composition runtime for facets, services, replicated state, and a
//! pluggable remote-service boundary. A Rust port of upstream `@earendil-works/chord`.

pub mod context;
pub mod delta;
pub mod error;
pub mod facets;
pub mod host2;
pub mod json;
pub mod node;
pub mod services;
pub mod state;
mod strict;
mod sync;
pub mod watch;

pub use context::{
    AbortController, AbortSignal, BACKGROUND_CONTEXT, Context, ContextKey, TODO_CONTEXT, await_with_context,
    create_context_key, trace_in, with_abort_signal, with_cancel, with_context_value, with_telemetry,
    without_abort_signal,
};
pub use error::{
    ChordError, ChordResult, REMOTE_SERVICE_ERROR_CODES, RemoteServiceError, RemoteServiceErrorCode,
    is_remote_service_error_code, report_unhandled, set_unhandled_error_hook,
};
pub use json::{Json, JsonMap, Number};
pub use state::{
    AttachedReplicatedState, DeliveryKind, ErrorReporter, MutableReplicatedState, ReplicatedState,
    ReplicatedStateDelivery, ReplicatedStateReplica, ReplicatedStateSource, ReplicatedStateSourceAttachment,
    ReplicatedStateSourceFrame, ReplicatedStateSourceOptions, ReplicatedStateSourceSnapshot, StateListener,
    StateMember, attach_replicated_state_source, replicated_state,
};
pub use strict::{CopyJsonOptions, copy_json, is_json_value};
pub use sync::{BoxFuture, Unsubscribe};
pub use watch::{WatchEnd, WatchHandle};

#[doc(hidden)]
pub mod __private {
    pub use serde_json;
}
