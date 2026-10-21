//! Port of upstream's chord2 host runtime (`packages/chord/src`), beside the older facets/services runtime.
//! Phase 1 holds the foundation: errors, schemas, contracts, wire frames and links. Later phases add cells,
//! objects and hosts, then connections, remotes and WebSockets, then reload and clusters, then the manager.

pub mod contract;
pub mod declare;
pub mod errors;
pub mod link;
pub mod publish;
pub mod registry;
pub mod schema;
pub mod wire;

pub use contract::{
    Accepts, Contract, Description, LOCAL_VARIANT, LocalDef, Member, MemberDescription, Members, MethodSpec,
    OpaqueSpec, Placement, RESERVED_MEMBERS, RemoteDef, Scope, VariantDef, bind, contract_from_description, coverage,
    host_family, host_named, local, method, opaque, placed_on, placement_text, remote, resolve_host, takes_no_args,
};
pub use errors::{
    CHORD_ERROR_CODES, ChordErrorCode, ErrorOptions, as_chord_error, forbidden, invalid, is_chord_error, run_all,
};
pub use link::{Link, MemoryLink, PACK_THRESHOLD, Packed, Received, memory_link_pair, unpack};
pub use publish::{enqueue, hold_publications};
pub use registry::{Conflict, Registration, Registry, RegistryTable, RegistryView, define_registry};
pub use schema::{assert_valid, canonical_identity, canonical_json, fits, hash64, normalize};
pub use wire::{Frame, PROTOCOL_VERSION, Payload, WireError, from_wire_error, is_frame, to_wire_error};
