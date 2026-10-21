//! One connection's state: its link, peer, flow control counters, and what each side asked of the other.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use indexmap::IndexMap;

use super::net::{ConnId, RelayId, Reply};
use super::relay::Relay;
use super::{
    ConnectionInfo, DEFAULT_PING_TIMEOUT, DEFAULT_WINDOW, LinkOptions, MismatchHandler, PENDING_WINDOWS, Restamp, Role,
    ServedInfo, Waiting,
};
use crate::context::{AbortController, AbortSignal};
use crate::delta::Encoder;
use crate::error::ChordError;
use crate::host2::errors::ChordErrorCode;
use crate::host2::link::Link;
use crate::host2::object::RefImpl;
use crate::host2::remote::RemoteBinding;
use crate::host2::wire::{Frame, PeerKind, StateFrame, WireReloadReport};
use crate::json::Json;
use crate::sync::Unsubscribe;

/// What a relay checks from outside the task: whether its origin link still has the same authorization.
pub(crate) struct ConnShared {
    pub link: Arc<dyn Link>,
    /// Bumped when the identity of the link is revoked or restamped: relays from before are refused.
    pub generation: AtomicU64,
}

/// A state member's stream to the consumer: its subscription and delta encoder.
pub(super) struct Feed {
    pub id: u64,
    pub unsubscribe: Unsubscribe,
    pub encoder: Encoder,
}

/// A hold the peer made on this host and serves locally.
pub(super) struct ServedLocal {
    pub seq: u64,
    pub reference: Arc<RefImpl>,
    pub principal: Json,
    pub origin: Option<String>,
    pub wants_state: bool,
    /// The consumer had values before (re-hold): its first unit waits for the window like any other.
    pub again: bool,
    pub unit_sent: bool,
    /// The shape change the last unit answered (see `RefImpl::live_seq`).
    pub shape_seq: u64,
    /// The members whose snapshot the consumer has, with their streams.
    pub feeds: IndexMap<String, Feed>,
    pub listeners: Vec<Unsubscribe>,
    /// Calls that came before the first unit: they start after it, as upstream's queued calls resume after it.
    pub held: Vec<(crate::host2::wire::Call, AbortController)>,
}

pub(super) enum Served {
    Local(Box<ServedLocal>),
    Relay(RelayId),
}

/// How a waiting stream recovers when its ops are dropped: the provider resends the current value as a reset
/// at flush; a relay (which has no value) asks downstream for one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Overflow {
    Resend,
    Resync(RelayId),
}

pub(super) struct StateSlot {
    pub hold: u64,
    pub m: String,
    pub base: Option<StateFrame>,
    pub ops: Vec<Vec<Json>>,
    pub size: usize,
    pub lazy: bool,
    pub overflow: Overflow,
}

/// What waits on a link: a hold's lazy unit, or a member's stream (see `flow.rs`).
pub(super) enum Slot {
    Unit { hold: u64 },
    State(Box<StateSlot>),
}

impl Slot {
    pub fn hold(&self) -> u64 {
        match self {
            Self::Unit { hold } => *hold,
            Self::State(slot) => slot.hold,
        }
    }
}

pub(super) struct PendingCall {
    pub reply: Reply<Option<Json>>,
    pub signal: AbortSignal,
    pub listener: u64,
}

/// Waits for a downstream host peer to run this host's code version.
pub(super) enum SyncWaiter {
    Reply(Reply<ConnId>),
    Hold(Arc<RemoteBinding>),
}

pub(super) struct Peer {
    pub kind: PeerKind,
    pub name: Option<String>,
}

pub(super) struct ConnConfig {
    pub role: Role,
    pub principal: Json,
    pub peer: Peer,
    pub origin_id: Option<String>,
    pub restamp: Option<Restamp>,
    pub max_calls: Option<usize>,
    pub options: LinkOptions,
}

pub(crate) struct Conn {
    pub id: ConnId,
    pub link: Arc<dyn Link>,
    pub shared: Arc<ConnShared>,
    pub role: Role,
    pub peer: Peer,
    /// A client connection's id: holds over it (directly or relayed) carry it as their `origin`.
    pub origin_id: Option<String>,
    pub principal: Json,
    pub restamp: Option<Restamp>,
    pub peer_code_version: Option<u64>,
    pub synced: bool,
    /// Code versions this child announced while behind this host (fenced): this host never ran them.
    pub fenced: HashSet<u64>,
    /// Roots of fenced code versions, by the code version of this host they were admitted in.
    pub admitted: HashMap<String, i64>,
    pub parent_name: Option<String>,
    pub sync_waiters: Vec<SyncWaiter>,
    next_id: u64,
    pub last_seen: Instant,
    pub ping_timeout: Duration,
    pub pings: i64,
    pub pinger: Option<tokio::task::AbortHandle>,
    pub max_calls: usize,
    pub window: usize,
    pub pending_cap: usize,
    /// Whether the peer acknowledges frames; until its hello is here, frames are counted in case it does.
    pub peer_acks: Option<bool>,
    pub sent: u64,
    pub acked: u64,
    pub unacked: VecDeque<usize>,
    pub unacked_bytes: usize,
    pub pending: IndexMap<String, Slot>,
    pub pending_bytes: usize,
    pub received: u64,
    pub ack_scheduled: bool,
    pub on_mismatch: Option<MismatchHandler>,
    pub mismatched: bool,
    /// Set while its close is handled: nothing more is processed for it.
    pub closing: bool,
    pub remotes: HashMap<u64, Arc<RemoteBinding>>,
    pub calls: HashMap<u64, PendingCall>,
    pub served: IndexMap<u64, Served>,
    pub incoming: HashMap<u64, AbortController>,
    pub relayed_refs: HashMap<u64, RelayId>,
    pub relayed_calls: HashMap<u64, (RelayId, u64)>,
    /// Reload votes awaited from this (child) peer.
    pub votes: HashMap<u64, Reply<Option<WireReloadReport>>>,
}

impl Conn {
    pub(super) fn new(id: ConnId, link: Arc<dyn Link>, config: ConnConfig) -> Self {
        let window = config.options.window.unwrap_or(DEFAULT_WINDOW);
        let synced = !(config.role == Role::Downstream && config.peer.kind == PeerKind::Host);
        Self {
            id,
            shared: Arc::new(ConnShared { link: link.clone(), generation: AtomicU64::new(0) }),
            link,
            role: config.role,
            peer: config.peer,
            origin_id: config.origin_id,
            principal: config.principal,
            restamp: config.restamp,
            peer_code_version: None,
            synced,
            fenced: HashSet::new(),
            admitted: HashMap::new(),
            parent_name: None,
            sync_waiters: Vec::new(),
            next_id: 1,
            last_seen: Instant::now(),
            ping_timeout: config.options.ping_timeout.unwrap_or(DEFAULT_PING_TIMEOUT),
            pings: 0,
            pinger: None,
            max_calls: config.max_calls.unwrap_or(usize::MAX),
            window,
            pending_cap: config.options.pending_cap.unwrap_or_else(|| window.saturating_mul(PENDING_WINDOWS)),
            peer_acks: None,
            sent: 0,
            acked: 0,
            unacked: VecDeque::new(),
            unacked_bytes: 0,
            pending: IndexMap::new(),
            pending_bytes: 0,
            received: 0,
            ack_scheduled: false,
            on_mismatch: config.options.on_protocol_mismatch,
            mismatched: false,
            closing: false,
            remotes: HashMap::new(),
            calls: HashMap::new(),
            served: IndexMap::new(),
            incoming: HashMap::new(),
            relayed_refs: HashMap::new(),
            relayed_calls: HashMap::new(),
            votes: HashMap::new(),
        }
    }

    pub fn open(&self) -> bool {
        !self.closing && !self.link.closed()
    }

    pub fn downstream(&self) -> bool {
        self.role == Role::Downstream
    }

    /// A downstream link to a child host.
    pub fn child_host(&self) -> bool {
        self.role == Role::Downstream && self.peer.kind == PeerKind::Host
    }

    /// The peer's name: configured for downstream hosts, announced by the parent upstream.
    pub fn name(&self) -> Option<&str> {
        match self.role {
            Role::Upstream => self.parent_name.as_deref(),
            Role::Downstream => self.peer.name.as_deref(),
        }
    }

    pub fn shown_name(&self) -> String {
        self.name().unwrap_or("peer").to_string()
    }

    pub fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Sends a frame, counting it for flow control.
    pub fn send(&mut self, frame: &Frame) {
        let size = self.link.send(frame).unwrap_or(0);
        if self.peer_acks == Some(false) {
            return;
        }
        self.sent += 1;
        self.unacked.push_back(size);
        self.unacked_bytes += size;
    }

    pub fn full(&self) -> bool {
        self.peer_acks == Some(true) && self.unacked_bytes >= self.window
    }

    pub fn info(&self, relays: &HashMap<RelayId, Relay>) -> ConnectionInfo {
        let served = self.served.values().map(|served| match served {
            Served::Local(local) => ServedInfo { relay: false, origin: local.origin.clone() },
            Served::Relay(relay) => ServedInfo { relay: true, origin: relays.get(relay).and_then(|r| r.tag.clone()) },
        });
        ConnectionInfo {
            role: self.role,
            peer_kind: self.peer.kind,
            name: self.name().map(str::to_string),
            open: self.open(),
            waiting: Waiting { slots: self.pending.len(), bytes: self.pending_bytes },
            served: served.collect(),
            remotes: self.remotes.len(),
            relayed_refs: self.relayed_refs.len(),
            relayed_calls: self.relayed_calls.len(),
        }
    }
}

pub(super) fn unavailable(message: impl AsRef<str>) -> ChordError {
    ChordError::code_only(ChordErrorCode::Unavailable, message)
}

pub(super) fn fail_waiter(waiter: SyncWaiter, error: ChordError) {
    match waiter {
        SyncWaiter::Reply(reply) => {
            let _ = reply.send(Err(error));
        }
        SyncWaiter::Hold(binding) => binding.gone(error),
    }
}
