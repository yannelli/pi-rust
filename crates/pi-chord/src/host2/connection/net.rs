//! The task that owns a host's connections and relays. Link callbacks, references, calls and the host post
//! messages to it; it handles them one at a time, so the frames of a link are processed in order.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Weak};

use indexmap::IndexMap;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

use super::conn::Conn;
use super::relay::Relay;
use super::{AcceptOptions, ConnectOptions, ConnectionInfo};
use crate::context::AbortSignal;
use crate::error::{ChordError, ChordResult};
use crate::host2::exec::panic_error;
use crate::host2::host::{HostImpl, PrincipalMatch};
use crate::host2::link::{Link, Received};
use crate::host2::object::HostCore;
use crate::host2::remote::RemoteBinding;
use crate::host2::types::StateEvent;
use crate::host2::wire::{Frame, Payload, RootRef};
use crate::json::Json;

pub(crate) type ConnId = u64;
pub(crate) type RelayId = u64;

pub(crate) type Reply<T> = oneshot::Sender<ChordResult<T>>;

/// Work run on the link task, with its state.
pub(crate) struct Work(pub(super) Box<dyn FnOnce(&mut Net) + Send>);

/// What the task does next. `seq` fields name one served hold or feed: a message for a replaced one is dropped.
pub(crate) enum Msg {
    /// `host`: kept alive from now on while it has connections.
    Accept {
        link: Arc<dyn Link>,
        options: AcceptOptions,
        host: Arc<HostImpl>,
    },
    Connect {
        link: Arc<dyn Link>,
        options: ConnectOptions,
        host: Arc<HostImpl>,
    },
    /// A link a route resolved: its connection once the peer is usable.
    Attach {
        link: Arc<dyn Link>,
        name: String,
        principal: Json,
        reply: Reply<ConnId>,
    },
    Received {
        conn: ConnId,
        received: Received,
    },
    Closed {
        conn: ConnId,
        reason: String,
    },
    Ack {
        conn: ConnId,
    },
    Ping {
        conn: ConnId,
    },
    /// A new binding: hold it over the link to its host.
    Hold {
        binding: Arc<RemoteBinding>,
    },
    HoldOn {
        conn: ConnId,
        binding: Arc<RemoteBinding>,
    },
    Release {
        conn: ConnId,
        id: u64,
    },
    WantState {
        conn: ConnId,
        hold: u64,
        wanted: bool,
    },
    Call {
        conn: ConnId,
        call: OutCall,
        reply: Reply<Option<Json>>,
    },
    CallAborted {
        conn: ConnId,
        id: u64,
        reason: ChordError,
    },
    Unit {
        conn: ConnId,
        hold: u64,
        seq: u64,
        shape_seq: u64,
    },
    ServedGone {
        conn: ConnId,
        hold: u64,
        seq: u64,
        error: ChordError,
    },
    Feed {
        conn: ConnId,
        hold: u64,
        name: String,
        feed: u64,
        event: StateEvent,
    },
    CallDone {
        conn: ConnId,
        id: u64,
        result: ChordResult<Option<Payload>>,
    },
    RelayOpen {
        relay: RelayId,
        down: ConnId,
    },
    RelayFailed {
        relay: RelayId,
        error: ChordError,
    },
    RelayCall {
        relay: RelayId,
        call: RelayedCall,
        reply: Reply<Option<Payload>>,
    },
    RelayCancel {
        relay: RelayId,
        call: u64,
        error: ChordError,
    },
    Send {
        conn: ConnId,
        frame: Frame,
    },
    Invalidate {
        filter: PrincipalMatch,
        done: oneshot::Sender<()>,
    },
    CloseAll {
        reason: String,
        done: oneshot::Sender<()>,
    },
    Inspect {
        reply: oneshot::Sender<Vec<ConnectionInfo>>,
    },
    /// Work on the connections themselves (reloads: participants, prepare and vote, commit, retire).
    Run(Work),
}

/// A call a remote reference makes over a hold.
pub(crate) struct OutCall {
    pub hold: u64,
    pub member: String,
    pub args: Option<Json>,
    pub root: Option<RootRef>,
    pub signal: AbortSignal,
}

/// A call the origin of a relay makes, to forward downstream.
pub(crate) struct RelayedCall {
    pub id: u64,
    pub member: String,
    pub args: Option<Payload>,
    pub root: RootRef,
    pub signal: AbortSignal,
}

/// Posts to a host's connection task.
#[derive(Clone)]
pub(crate) struct NetHandle(mpsc::UnboundedSender<Msg>);

impl NetHandle {
    pub(crate) fn post(&self, msg: Msg) {
        let _ = self.0.send(msg);
    }
}

/// Posts while the host lives: links and timers never keep the task alive.
#[derive(Clone)]
pub(crate) struct WeakNet(mpsc::WeakUnboundedSender<Msg>);

impl WeakNet {
    /// False once the task is gone.
    pub(crate) fn post(&self, msg: Msg) -> bool {
        self.0.upgrade().is_some_and(|sender| sender.send(msg).is_ok())
    }
}

pub(crate) struct Net {
    pub host: Weak<HostImpl>,
    /// The host stays alive while it has connections, as upstream's does while its links are open.
    keep: Option<Arc<HostImpl>>,
    pub me: WeakNet,
    pub handle: Handle,
    pub conns: IndexMap<ConnId, Conn>,
    pub relays: HashMap<RelayId, Relay>,
    pub upstream: Option<ConnId>,
    /// Child hosts whose hello arrived during a reload: synced or reconciled once it ends.
    pub deferred_hellos: IndexMap<ConnId, u64>,
    next_conn: ConnId,
    next_relay: RelayId,
    next_seq: u64,
    pub origin_seq: u64,
}

/// Starts the connection task of a host, on its runtime.
pub(crate) fn spawn_net(host: Weak<HostImpl>, handle: Option<&Handle>) -> NetHandle {
    let handle = handle.cloned().or_else(|| Handle::try_current().ok());
    let handle = handle.expect("chord2 links need a Tokio runtime: create the host inside one");
    let (sender, receiver) = mpsc::unbounded_channel();
    let net = Net {
        host,
        keep: None,
        me: WeakNet(sender.downgrade()),
        handle: handle.clone(),
        conns: IndexMap::new(),
        relays: HashMap::new(),
        upstream: None,
        deferred_hellos: IndexMap::new(),
        next_conn: 0,
        next_relay: 0,
        next_seq: 0,
        origin_seq: 0,
    };
    handle.spawn(net.run(receiver));
    NetHandle(sender)
}

impl Net {
    async fn run(mut self, mut receiver: mpsc::UnboundedReceiver<Msg>) {
        while let Some(msg) = receiver.recv().await {
            if let Err(panic) = catch_unwind(AssertUnwindSafe(|| self.handle(msg))) {
                self.report(panic_error(panic.as_ref()));
            }
            self.keep = match self.conns.is_empty() {
                true => None,
                false => self.keep.take().or_else(|| self.host.upgrade()),
            };
        }
    }

    pub(super) fn host(&self) -> Option<Arc<HostImpl>> {
        self.host.upgrade()
    }

    pub(super) fn report(&self, error: ChordError) {
        if let Some(host) = self.host() {
            HostCore::report(host.as_ref(), error);
        }
    }

    pub(super) fn seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    pub(super) fn conn_id(&mut self) -> ConnId {
        self.next_conn += 1;
        self.next_conn
    }

    pub(super) fn relay_id(&mut self) -> RelayId {
        self.next_relay += 1;
        self.next_relay
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Accept { link, options, host } => {
                self.keep = Some(host);
                self.accept(link, options);
            }
            Msg::Connect { link, options, host } => {
                self.keep = Some(host);
                self.connect(link, options);
            }
            Msg::Attach { link, name, principal, reply } => self.attach(link, name, principal, reply),
            Msg::Received { conn, received } => self.receive(conn, received),
            Msg::Closed { conn, reason } => self.closed(conn, &reason),
            Msg::Ack { conn } => self.send_ack(conn),
            Msg::Ping { conn } => self.ping(conn),
            Msg::Hold { binding } => self.hold_binding(binding),
            Msg::HoldOn { conn, binding } => {
                if !binding.ended() && binding.hold().is_none() {
                    self.hold_on(conn, &binding, false);
                }
            }
            Msg::Release { conn, id } => self.release_remote(conn, id),
            Msg::WantState { conn, hold, wanted } => self.want_remote_state(conn, hold, wanted),
            Msg::Call { conn, call, reply } => self.call(conn, call, reply),
            Msg::CallAborted { conn, id, reason } => self.call_aborted(conn, id, reason),
            Msg::Unit { conn, hold, seq, shape_seq } => self.shape_unit(conn, hold, seq, shape_seq),
            Msg::ServedGone { conn, hold, seq, error } => self.served_gone(conn, hold, seq, &error),
            Msg::Feed { conn, hold, name, feed, event } => self.feed_event(conn, hold, &name, feed, &event),
            Msg::CallDone { conn, id, result } => self.call_done(conn, id, result),
            Msg::RelayOpen { relay, down } => self.relay_resolved(relay, down),
            Msg::RelayFailed { relay, error } => self.relay_close(relay, &error, true, true),
            Msg::RelayCall { relay, call, reply } => self.relay_call(relay, call, reply),
            Msg::RelayCancel { relay, call, error } => self.relay_cancel(relay, call, error),
            Msg::Send { conn, frame } => self.send(conn, &frame),
            Msg::Invalidate { filter, done } => {
                self.invalidate(&filter);
                let _ = done.send(());
            }
            Msg::CloseAll { reason, done } => {
                let ids: Vec<ConnId> = self.conns.keys().copied().collect();
                for id in ids {
                    self.close_link(id, &reason);
                }
                let _ = done.send(());
            }
            Msg::Inspect { reply } => {
                let _ = reply.send(self.conns.values().map(|conn| conn.info(&self.relays)).collect());
            }
            Msg::Run(Work(work)) => work(self),
        }
    }

    /// `invalidate_principal`: links of a matching identity are restamped (if the acceptor can) or closed, and
    /// patterns reach child hosts.
    fn invalidate(&mut self, filter: &PrincipalMatch) {
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        for id in ids {
            let Some(conn) = self.conns.get(&id) else { continue };
            if !conn.downstream() || !filter.matches(&conn.principal) {
                continue;
            }
            let next = conn.restamp.as_ref().and_then(|restamp| restamp(&conn.principal));
            self.revoke(id, next);
        }
        if let PrincipalMatch::Pattern(pattern) = filter {
            let pattern = pattern.as_object().cloned().unwrap_or_default();
            let children: Vec<ConnId> =
                self.conns.values().filter(|conn| conn.child_host()).map(|conn| conn.id).collect();
            for id in children {
                self.send(id, &Frame::Invalidate { pattern: pattern.clone() });
            }
        }
    }
}
