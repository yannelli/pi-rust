//! Adding connections (accept, connect, links a route resolved), waiting for a peer to be usable, keepalive pings
//! and acknowledgements.

use std::sync::Arc;

use super::conn::{Conn, ConnConfig, Peer, SyncWaiter, fail_waiter, unavailable};
use super::net::{ConnId, Msg, Net, Reply};
use super::{AcceptOptions, ConnectOptions, DEFAULT_PING_INTERVAL, LinkOptions, Role};
use crate::host2::link::Link;
use crate::host2::object::HostCore;
use crate::host2::wire::{Frame, Hello, PROTOCOL_VERSION, PeerKind};
use crate::json::Json;

impl Net {
    /// Adds a connection over `link`. `None`: the link was closed already (keeping it would route to it forever).
    pub(super) fn add(&mut self, link: Arc<dyn Link>, config: ConnConfig) -> Option<ConnId> {
        let host = self.host()?;
        let id = self.conn_id();
        let interval = config.options.ping_interval.unwrap_or(DEFAULT_PING_INTERVAL);
        let me = self.me.clone();
        link.on_close(Box::new(move |reason| {
            me.post(Msg::Closed { conn: id, reason: reason.to_string() });
        }));
        let me = self.me.clone();
        link.receive(Arc::new(move |received| {
            me.post(Msg::Received { conn: id, received });
        }));
        if link.closed() {
            return None;
        }
        let mut conn = Conn::new(id, link, config);
        conn.send(&Frame::Hello(Hello {
            protocol: Some(PROTOCOL_VERSION),
            name: host.name().to_string(),
            kind: host.kind(),
            code_version: host.current_code_version() as u64,
            acks: Some(true),
        }));
        let me = self.me.clone();
        let pinger = self.handle.spawn(async move {
            let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
            loop {
                ticker.tick().await;
                if !me.post(Msg::Ping { conn: id }) {
                    return;
                }
            }
        });
        conn.pinger = Some(pinger.abort_handle());
        self.conns.insert(id, conn);
        Some(id)
    }

    pub(super) fn send(&mut self, conn: ConnId, frame: &Frame) {
        if let Some(conn) = self.conns.get_mut(&conn) {
            conn.send(frame);
        }
    }

    /// Closes a link and handles its close now (the close handler's message comes later and is ignored).
    pub(super) fn close_link(&mut self, conn: ConnId, reason: &str) {
        let Some(link) = self.conns.get(&conn).map(|conn| conn.link.clone()) else { return };
        link.close(Some(reason));
        self.closed(conn, reason);
    }

    pub(super) fn ping(&mut self, id: ConnId) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        if conn.last_seen.elapsed() > conn.ping_timeout {
            self.close_link(id, "ping timeout");
            return;
        }
        conn.pings += 1;
        let n = conn.pings;
        conn.send(&Frame::Ping { n });
    }

    /// Acknowledges once the frames that arrived together are processed: one ack per burst.
    pub(super) fn schedule_ack(&mut self, id: ConnId) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        if conn.peer_acks != Some(true) || conn.ack_scheduled || !conn.open() {
            return;
        }
        conn.ack_scheduled = true;
        self.me.post(Msg::Ack { conn: id });
    }

    pub(super) fn send_ack(&mut self, id: ConnId) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        conn.ack_scheduled = false;
        if conn.open() {
            conn.link.send(&Frame::Ack { n: conn.received });
        }
    }

    pub(super) fn accept(&mut self, link: Arc<dyn Link>, options: AcceptOptions) {
        let Some(host) = self.host() else { return };
        let origin_id = (options.kind == PeerKind::Client).then(|| {
            self.origin_seq += 1;
            format!("{}:{}", host.incarnation(), self.origin_seq)
        });
        let name = if options.kind == PeerKind::Host { options.name } else { None };
        let config = ConnConfig {
            role: Role::Downstream,
            principal: options.principal,
            peer: Peer { kind: options.kind, name },
            origin_id,
            restamp: options.restamp,
            max_calls: options.max_calls,
            options: options.link,
        };
        self.add(link, config);
    }

    pub(super) fn connect(&mut self, link: Arc<dyn Link>, options: ConnectOptions) {
        if let Some(upstream) = self.upstream.filter(|id| self.conns.get(id).is_some_and(Conn::open)) {
            self.close_link(upstream, "replaced");
        }
        let config = ConnConfig {
            role: Role::Upstream,
            principal: options.principal,
            peer: Peer { kind: PeerKind::Host, name: None },
            origin_id: None,
            restamp: None,
            max_calls: None,
            options: options.link,
        };
        let Some(id) = self.add(link, config) else { return };
        self.upstream = Some(id);
        let Some(host) = self.host() else { return };
        // Bindings that lost their link re-hold over the new one (`again`: they keep their values meanwhile).
        for binding in host.bindings() {
            if binding.hold().is_none() && !binding.ended() {
                self.hold_on(id, &binding, binding.applied());
            }
        }
    }

    /// The connection to a resolved link: an existing one, or a new downstream host connection.
    pub(super) fn attach(&mut self, link: Arc<dyn Link>, name: String, principal: Json, reply: Reply<ConnId>) {
        let existing = self.conns.values().find(|conn| std::ptr::addr_eq(Arc::as_ptr(&conn.link), Arc::as_ptr(&link)));
        let id = match existing {
            Some(conn) => Some(conn.id),
            None => {
                let config = ConnConfig {
                    role: Role::Downstream,
                    principal,
                    peer: Peer { kind: PeerKind::Host, name: Some(name.clone()) },
                    origin_id: None,
                    restamp: None,
                    max_calls: None,
                    options: LinkOptions::default(),
                };
                self.add(link, config)
            }
        };
        match id {
            Some(id) => self.when_synced(id, SyncWaiter::Reply(reply)),
            None => {
                let _ = reply.send(Err(unavailable(format!("link to {name} closed"))));
            }
        }
    }

    /// Runs `waiter` once the peer is usable (a host peer runs this host's code version), or fails it.
    pub(super) fn when_synced(&mut self, id: ConnId, waiter: SyncWaiter) {
        let Some(conn) = self.conns.get_mut(&id) else {
            return fail_waiter(waiter, unavailable("link to peer closed"));
        };
        if !conn.open() {
            let error = unavailable(format!("link to {} closed", conn.shown_name()));
            return fail_waiter(waiter, error);
        }
        if !conn.synced {
            conn.sync_waiters.push(waiter);
            return;
        }
        self.synced_waiter(id, waiter);
    }

    fn synced_waiter(&mut self, id: ConnId, waiter: SyncWaiter) {
        match waiter {
            SyncWaiter::Reply(reply) => {
                let _ = reply.send(Ok(id));
            }
            SyncWaiter::Hold(binding) => {
                if !binding.ended() && binding.hold().is_none() {
                    self.hold_on(id, &binding, false);
                }
            }
        }
    }

    pub(super) fn mark_synced(&mut self, id: ConnId, code_version: u64) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        conn.peer_code_version = Some(code_version);
        conn.synced = true;
        let waiters = std::mem::take(&mut conn.sync_waiters);
        for waiter in waiters {
            self.synced_waiter(id, waiter);
        }
    }
}
