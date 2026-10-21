//! Port of upstream's chord2 host runtime (`packages/chord/src`), beside the older facets/services runtime: the
//! foundation (errors, schemas, contracts, wire frames, links), the in-host runtime, connections, remote references,
//! WebSocket links, reloads, retirement and clusters. Upstream's `Self` is [`object::Declare`] here.

pub mod cell;
pub mod connection;
pub mod contract;
pub mod declare;
pub mod errors;
pub(crate) mod exec;
pub mod host;
pub mod link;
pub mod loader;
pub mod manager;
pub mod object;
pub mod process_lock;
pub mod publish;
pub mod registry;
pub mod reload;
pub(crate) mod remote;
pub mod schema;
pub mod types;
pub mod websocket;
pub mod wire;

pub use cell::{Cell, Derived};
pub use connection::{
    AcceptOptions, Authorize, ConnectOptions, ConnectionInfo, LinkOptions, MismatchHandler, ProtocolVersions, Resolve,
    Restamp, Role, Route, ServedInfo, Waiting,
};

pub use contract::{
    Accepts, Contract, Description, LOCAL_VARIANT, LocalDef, Member, MemberDescription, Members, MethodSpec,
    OpaqueSpec, Placement, RESERVED_MEMBERS, RemoteDef, Scope, VariantDef, bind, contract_from_description, coverage,
    host_family, host_named, local, method, opaque, placed_on, placement_text, remote, resolve_host, takes_no_args,
};
pub use errors::{
    CHORD_ERROR_CODES, ChordErrorCode, ErrorOptions, as_chord_error, forbidden, invalid, is_chord_error, run_all,
};
pub use host::{
    Facet, FacetApi, Host, HostOptions, HostRegistry, LingerOptions, LoadOptions, PluginRef, PrincipalMatch,
    create_host, define_facet,
};
pub use link::{
    IpcChannel, IpcLink, Link, MemoryLink, PACK_THRESHOLD, Packed, Received, StreamChannel, WebSocketLink,
    WebSocketLinkOptions, connect_web_socket, ipc_link, memory_link_pair, unpack, web_socket_link,
};
pub use loader::{FacetLoader, InProcessLoader};
pub use manager::{
    ChildControl, ControlMessage, Driver, Exit, ForkDriver, ForkDriverOptions, HostManager, HostManagerOptions,
    Instance, Liveness, Status,
};
pub use object::{
    Ctx, Declare, Implementation, Initial, Lifetime, Listen, Listenable, Ref, RefCore, RefOwner, ResourceHandle,
    RootOptions, ServeOptions, StateOptions,
};
pub use process_lock::{ProcessLock, process_lock};
pub use publish::{enqueue, hold_publications};
pub use registry::{Conflict, Registration, Registry, RegistryTable, RegistryView, define_registry};
pub use reload::{ReloadOptions, ReloadReport};
pub use schema::{assert_valid, canonical_identity, canonical_json, fits, hash64, normalize};
pub use types::{GetOptions, Readable, ReadableRef, RefStatus, Shape, StateEvent, on_event, on_value};
pub use websocket::{Authenticator, WebSocketAcceptOptions, accept_web_sockets};
pub use wire::{Frame, PROTOCOL_VERSION, Payload, WireError, from_wire_error, is_frame, to_wire_error};
