//! Port of upstream's chord2 host runtime (`packages/chord/src`), beside the older facets/services runtime.
//! Phase 1 holds the foundation: errors, schemas, contracts, wire frames and links. Phase 2 adds cells, objects
//! and the in-host runtime. Upstream's `Self` (what a factory declares through) is [`object::Declare`] here.

pub mod cell;
pub mod contract;
pub mod declare;
pub mod errors;
pub(crate) mod exec;
pub mod host;
pub mod link;
pub mod object;
pub mod publish;
pub mod registry;
pub mod schema;
pub mod types;
pub mod wire;

pub use cell::{Cell, Derived};

pub use contract::{
    Accepts, Contract, Description, LOCAL_VARIANT, LocalDef, Member, MemberDescription, Members, MethodSpec,
    OpaqueSpec, Placement, RESERVED_MEMBERS, RemoteDef, Scope, VariantDef, bind, contract_from_description, coverage,
    host_family, host_named, local, method, opaque, placed_on, placement_text, remote, resolve_host, takes_no_args,
};
pub use errors::{
    CHORD_ERROR_CODES, ChordErrorCode, ErrorOptions, as_chord_error, forbidden, invalid, is_chord_error, run_all,
};
pub use host::{
    Facet, FacetApi, Host, HostOptions, HostRegistry, LoadOptions, PrincipalMatch, create_host, define_facet,
};
pub use link::{Link, MemoryLink, PACK_THRESHOLD, Packed, Received, memory_link_pair, unpack};
pub use object::{
    Ctx, Declare, Implementation, Initial, Lifetime, Listen, Listenable, Ref, RefCore, RefOwner, ResourceHandle,
    RootOptions, ServeOptions, StateOptions,
};
pub use publish::{enqueue, hold_publications};
pub use registry::{Conflict, Registration, Registry, RegistryTable, RegistryView, define_registry};
pub use schema::{assert_valid, canonical_identity, canonical_json, fits, hash64, normalize};
pub use types::{GetOptions, Readable, ReadableRef, RefStatus, Shape, StateEvent, on_event, on_value};
pub use wire::{Frame, PROTOCOL_VERSION, Payload, WireError, from_wire_error, is_frame, to_wire_error};
