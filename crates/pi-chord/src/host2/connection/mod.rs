//! Port of upstream `connection.ts`: one end of a link on a host. It consumes remote references (holds, calls,
//! state), serves the peer's holds, relays holds to other links, and applies flow control. A host's connections
//! all live in one task ([`net`]), as upstream's live on one JavaScript thread: frames and requests are messages.

mod attach;
mod conn;
mod consume;
mod flow;
mod incoming;
mod net;
mod receive;
mod relay;
mod route;
mod serve;

use std::sync::Arc;
use std::time::Duration;

pub(crate) use net::{ConnId, Msg, NetHandle, OutCall, spawn_net};

use super::link::Link;
use super::wire::PeerKind;
use crate::error::ChordResult;
use crate::json::Json;
use crate::sync::BoxFuture;

/// The versions of a link whose peer speaks another protocol (`peer`: `None` if its hello did not say).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolVersions {
    pub local: u64,
    pub peer: Option<u64>,
}

pub type MismatchHandler = Arc<dyn Fn(ProtocolVersions) + Send + Sync>;
/// On `invalidate_principal`: a new identity keeps the link, `None` closes it.
pub type Restamp = Arc<dyn Fn(&Json) -> Option<Json> + Send + Sync>;

/// Options of a link, on either end.
#[derive(Clone, Default)]
pub struct LinkOptions {
    /// Time between pings (default 15 s).
    pub ping_interval: Option<Duration>,
    /// Close the link when nothing arrived for this long (default 45 s). Everything held through it is released.
    pub ping_timeout: Option<Duration>,
    /// Unacknowledged frame sizes beyond which state frames wait for the peer (default 256 KiB). Applies only when
    /// both ends acknowledge frames and the link reports sizes; `usize::MAX` never holds state back.
    pub window: Option<usize>,
    /// Bytes of state ops that may wait on this link (default 4 windows). Beyond, the oldest waiting streams are
    /// replaced by their current value (a reset), so memory stays bounded however long the peer stalls.
    pub pending_cap: Option<usize>,
    /// The peer speaks another link protocol version: the link closes, and should not be reconnected as is.
    pub on_protocol_mismatch: Option<MismatchHandler>,
}

/// `accept` options. The acceptor decides what the peer is, never its `hello`.
#[derive(Clone)]
pub struct AcceptOptions {
    /// The identity of whatever connects, from the app's authenticator. Requests on the link use it.
    pub principal: Json,
    /// `Client` (default: no roots, no code versions, not addressable) or `Host` (a child host; needs `name`).
    pub kind: PeerKind,
    /// A child host's name: its hello must match, and it is reachable at this name.
    pub name: Option<String>,
    pub restamp: Option<Restamp>,
    /// Calls the peer may have in progress (default: no limit). Further ones are answered at once with
    /// `unavailable/too_many_calls`, and the link stays open.
    pub max_calls: Option<usize>,
    pub link: LinkOptions,
}

impl AcceptOptions {
    /// A client link for this identity.
    pub fn client(principal: Json) -> Self {
        Self {
            principal,
            kind: PeerKind::Client,
            name: None,
            restamp: None,
            max_calls: None,
            link: LinkOptions::default(),
        }
    }

    /// A child host link named `name`, for this identity.
    pub fn host(name: &str, principal: Json) -> Self {
        Self { kind: PeerKind::Host, name: Some(name.to_string()), ..Self::client(principal) }
    }
}

/// `connect` options: the link's, and the identity of the parent's own requests.
#[derive(Clone)]
pub struct ConnectOptions {
    /// The identity of a request from the parent that does not forward its caller's identity.
    pub principal: Json,
    pub link: LinkOptions,
}

impl ConnectOptions {
    pub fn new(principal: Json) -> Self {
        Self { principal, link: LinkOptions::default() }
    }
}

pub type Authorize = Arc<dyn Fn(&Json, &str) -> BoxFuture<bool> + Send + Sync>;
pub type Resolve = Arc<dyn Fn(&str) -> BoxFuture<ChordResult<Arc<dyn Link>>> + Send + Sync>;

/// Where requests for hosts whose names start with a prefix go (on the server).
#[derive(Clone)]
pub struct Route {
    /// Checked before resolving (or spawning) and again after: grants may change meanwhile.
    pub authorize: Authorize,
    /// A link to the host, spawned if needed.
    pub resolve: Resolve,
    /// The identity of the resolved host's own calls back.
    pub principal: Arc<dyn Fn(&str) -> Json + Send + Sync>,
}

/// `Upstream`: the link to this host's parent. `Downstream`: a link this host accepted or resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Upstream,
    Downstream,
}

/// Unacknowledged state waiting for the peer: slots and bytes of ops.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Waiting {
    pub slots: usize,
    pub bytes: usize,
}

/// A hold the peer made on this host (diagnostics).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServedInfo {
    pub relay: bool,
    /// The client connection a local hold comes from.
    pub origin: Option<String>,
}

/// A snapshot of one connection (diagnostics and tests; upstream tests read these fields directly).
#[derive(Clone, Debug)]
pub struct ConnectionInfo {
    pub role: Role,
    pub peer_kind: PeerKind,
    /// The peer's name: configured downstream, announced by the parent upstream.
    pub name: Option<String>,
    pub open: bool,
    pub waiting: Waiting,
    pub served: Vec<ServedInfo>,
    pub remotes: usize,
    pub relayed_refs: usize,
    pub relayed_calls: usize,
}

const DEFAULT_WINDOW: usize = 256 * 1024;
/// The default cap of waiting state ops per link, in windows.
const PENDING_WINDOWS: usize = 4;
const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(15);
const DEFAULT_PING_TIMEOUT: Duration = Duration::from_secs(45);

fn kind_name(kind: PeerKind) -> &'static str {
    match kind {
        PeerKind::Host => "host",
        PeerKind::Client => "client",
    }
}
