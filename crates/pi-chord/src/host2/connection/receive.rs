//! Frames from the peer: validation, hellos, replies to holds and calls, control frames from the parent, and the
//! end of a link.

use std::time::Instant;

use super::conn::{fail_waiter, unavailable};
use super::kind_name;
use super::net::{ConnId, Net};
use super::{ProtocolVersions, Role};
use crate::error::ChordError;
use crate::host2::errors::{ChordErrorCode, ErrorOptions};
use crate::host2::exec::guarded;
use crate::host2::host::PrincipalMatch;
use crate::host2::link::{Received, unpack};
use crate::host2::object::HostCore;
use crate::host2::remote::StateIn;
use crate::host2::types::Shape;
use crate::host2::wire::{
    CallResult, Frame, Hello, PROTOCOL_VERSION, PeerKind, StateFrame, StateKind, from_wire_error, to_wire_error,
};
use crate::json::Json;

impl Net {
    pub(super) fn receive(&mut self, id: ConnId, received: Received) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        if conn.mismatched || conn.closing {
            return;
        }
        conn.last_seen = Instant::now();
        let frame = match received {
            Received::Frame(frame) => frame,
            Received::Malformed(_) => return self.violation(id, "malformed frame"),
        };
        if let Frame::Ack { n } = frame {
            return self.on_ack(id, n);
        }
        conn.received += 1;
        self.dispatch(id, frame);
        self.schedule_ack(id);
    }

    fn dispatch(&mut self, id: ConnId, frame: Frame) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        let role = conn.role;
        match frame {
            Frame::Hello(hello) => self.on_hello(id, hello),
            Frame::Ping { n } => conn.send(&Frame::Pong { n }),
            Frame::Pong { .. } | Frame::Bye { .. } | Frame::Ack { .. } => {}
            Frame::Hold(hold) => self.on_hold(id, hold),
            Frame::Release { id: hold } => self.drop_served(id, hold, true),
            Frame::Call(call) => self.on_call(id, call),
            Frame::Cancel { id: call } => {
                if let Some(controller) = conn.incoming.get(&call) {
                    controller.abort(Some(ChordError::code_only(ChordErrorCode::Aborted, "cancelled by caller")));
                }
            }
            Frame::Sub { hold } => self.on_want_state(id, hold, true),
            Frame::Unsub { hold } => self.on_want_state(id, hold, false),
            Frame::Resync { hold, m } => self.on_resync(id, hold, &m),
            Frame::Shape(shape) => self.on_shape(id, shape),
            Frame::Gone { id: hold, error } => self.on_gone(id, hold, from_wire_error(&error)),
            Frame::Result(result) => self.on_result(id, result),
            Frame::State(state) => self.on_state(id, state),
            Frame::RootEnd { root_id } => {
                // Honored only from the link the root came over (see `HostImpl::foreign_root_ended`).
                conn.admitted.remove(&root_id);
                if let Some(host) = self.host() {
                    host.foreign_root_ended(&root_id, id);
                }
            }
            frame @ (Frame::Invalidate { .. }
            | Frame::Prepare(_)
            | Frame::Commit { .. }
            | Frame::Abort { .. }
            | Frame::Retire { .. }) => {
                // Control frames only from the parent.
                if role != Role::Upstream {
                    return self.violation(id, &format!("{} from a child", frame.tag()));
                }
                self.on_control(id, frame);
            }
            Frame::Vote(vote) => {
                if !conn.child_host() {
                    return self.violation(id, "vote from a non-participant");
                }
                if let Some(waiter) = conn.votes.remove(&vote.reload) {
                    let _ = waiter.send(match vote.error {
                        Some(error) => Err(from_wire_error(&error)),
                        None => Ok(vote.report),
                    });
                }
            }
        }
    }

    fn on_control(&mut self, id: ConnId, frame: Frame) {
        let Some(host) = self.host() else { return };
        match frame {
            Frame::Invalidate { pattern } => {
                let done = host.invalidate(PrincipalMatch::Pattern(Json::object(pattern)));
                let report = host.clone();
                self.handle.spawn(async move {
                    if let Err(error) = done.await {
                        report.report(error);
                    }
                });
            }
            Frame::Retire { code_version } => host.allow_retire(code_version),
            frame @ (Frame::Prepare(_) | Frame::Commit { .. } | Frame::Abort { .. }) => host.on_reload_frame(id, frame),
            _ => {}
        }
    }

    /// The peer's hello must agree with what this side decided about it. A downstream host announces its code
    /// version: the host either finds it in step or fences it (never renumbers).
    fn on_hello(&mut self, id: ConnId, hello: Hello) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        let protocol = hello.protocol.unwrap_or(1);
        if protocol != PROTOCOL_VERSION {
            return self.protocol_mismatch(id, Some(protocol), true);
        }
        if conn.peer_acks.is_none() {
            let acks = hello.acks == Some(true);
            conn.peer_acks = Some(acks);
            if !acks {
                conn.unacked.clear();
                conn.unacked_bytes = 0;
            }
        }
        if conn.role == Role::Upstream {
            if hello.kind != PeerKind::Host {
                return self.violation(id, "parent is not a host");
            }
            conn.parent_name.get_or_insert(hello.name);
            return;
        }
        let named = conn.peer.name.as_ref().is_none_or(|name| *name == hello.name);
        if hello.kind != conn.peer.kind || !named {
            let accepted = format!("{} {}", kind_name(conn.peer.kind), conn.peer.name.as_deref().unwrap_or(""));
            let reason =
                format!("hello {} {} does not match the accepted {accepted}", kind_name(hello.kind), hello.name);
            return self.violation(id, &reason);
        }
        if conn.peer.kind == PeerKind::Host {
            self.peer_hello(id, hello.code_version);
        }
    }

    /// A child host announced its code version. In step: usable. Ahead: refused. Behind: fenced (nothing is
    /// routed to it) and reconciled by a reload of its modules to this host's code version, never renumbered.
    pub(super) fn peer_hello(&mut self, id: ConnId, code_version: u64) {
        let (Some(host), Some(conn)) = (self.host(), self.conns.get_mut(&id)) else { return };
        let current = host.current_code_version() as u64;
        if code_version > current {
            let message = format!("{} runs code version {code_version}, ahead of {current}", conn.shown_name());
            host.report(ChordError::code_only(ChordErrorCode::Invalid, message));
            return self.close_link(id, "code version ahead of parent");
        }
        if host.reloading() {
            // Not a participant of the running reload: decided when it ends. A child behind is fenced now, so the
            // calls its roots make meanwhile are admitted in this host's code version instead of `superseded`.
            conn.peer_code_version = Some(code_version);
            if code_version < current {
                conn.fenced.insert(code_version);
            }
            self.deferred_hellos.insert(id, code_version);
            return;
        }
        if code_version == current {
            self.mark_synced(id, code_version);
            self.announce_retired(id);
            return;
        }
        conn.peer_code_version = Some(code_version);
        conn.fenced.insert(code_version);
        self.reconcile(id);
    }

    /// Stops talking to a peer of another protocol version: reports it and closes the link. The close reason tells
    /// the peer this side's version (`protocol-version <n>`); a peer that noticed first told us the same way.
    pub(super) fn protocol_mismatch(&mut self, id: ConnId, peer: Option<u64>, close: bool) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        if conn.mismatched {
            return;
        }
        conn.mismatched = true;
        if let (Some(handler), Some(host)) = (conn.on_mismatch.clone(), self.host()) {
            let report = host.error_handler();
            guarded(&report, || handler(ProtocolVersions { local: PROTOCOL_VERSION, peer }));
        }
        if close {
            self.close_link(id, &format!("protocol-version {PROTOCOL_VERSION}"));
        }
    }

    /// A protocol violation: the peer cannot be trusted with anything on this link any more.
    pub(super) fn violation(&mut self, id: ConnId, reason: &str) {
        let Some(conn) = self.conns.get(&id) else { return };
        let message = format!("link to {}: {reason}", conn.shown_name());
        self.report(ChordError::coded(ChordErrorCode::Invalid, message, ErrorOptions::reason("protocol")));
        self.close_link(id, &format!("protocol error: {reason}"));
    }

    fn on_shape(&mut self, id: ConnId, frame: crate::host2::wire::Shape) {
        let Some(conn) = self.conns.get(&id) else { return };
        if let Some(relay) = conn.relayed_refs.get(&frame.id).copied() {
            return self.relay_shape(relay, frame);
        }
        let Some(binding) = conn.remotes.get(&frame.id).cloned() else { return };
        let shape = Shape { variant: frame.variant, members: frame.members, contract: frame.contract };
        binding.shaped(shape, frame.state);
    }

    fn on_gone(&mut self, id: ConnId, hold: u64, error: ChordError) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        if let Some(relay) = conn.relayed_refs.get(&hold).copied() {
            return self.relay_close(relay, &error, false, true);
        }
        if let Some(binding) = conn.remotes.remove(&hold) {
            binding.gone(error);
        }
    }

    fn on_result(&mut self, id: ConnId, result: CallResult) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        if let Some((relay, call)) = conn.relayed_calls.get(&result.id).copied() {
            return self.relay_finished(relay, call, result);
        }
        let Some(pending) = conn.calls.remove(&result.id) else { return };
        pending.signal.remove_listener(pending.listener);
        let outcome = match (result.error, result.value) {
            (Some(error), _) => Err(from_wire_error(&error)),
            (None, value) => value.map(|value| unpack(&value)).transpose(),
        };
        let _ = pending.reply.send(outcome);
    }

    fn on_state(&mut self, id: ConnId, frame: StateFrame) {
        let Some(conn) = self.conns.get(&id) else { return };
        if let Some(relay) = conn.relayed_refs.get(&frame.hold).copied() {
            return self.relay_state(relay, frame);
        }
        let Some(binding) = conn.remotes.get(&frame.hold).cloned() else { return };
        let value = match frame.value.as_ref().map(unpack).transpose() {
            Ok(value) => value,
            Err(error) => {
                let failed =
                    StateIn { kind: StateKind::Error, value: None, ops: None, error: Some(to_wire_error(&error)) };
                return binding.state(&frame.m, failed);
            }
        };
        binding.state(&frame.m, StateIn { kind: frame.kind, value, ops: frame.ops, error: frame.error });
    }

    /// The link closed: everything the peer held here is released, everything held through it is gone (or, for
    /// the parent's link, waits for the next one), and calls in either direction fail.
    pub(super) fn closed(&mut self, id: ConnId, reason: &str) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        if conn.closing {
            return;
        }
        let told = reason.strip_prefix("protocol-version ").and_then(|version| version.parse::<u64>().ok());
        if let Some(version) = told.filter(|version| *version != PROTOCOL_VERSION) {
            self.protocol_mismatch(id, Some(version), false);
        }
        let Some(conn) = self.conns.get_mut(&id) else { return };
        conn.closing = true;
        if let Some(pinger) = conn.pinger.take() {
            pinger.abort();
        }
        conn.pending.clear();
        conn.pending_bytes = 0;
        let error = unavailable(format!("link to {} closed: {reason}", conn.shown_name()));
        let served: Vec<u64> = conn.served.keys().copied().collect();
        for hold in served {
            self.drop_served(id, hold, true);
        }
        let Some(conn) = self.conns.get_mut(&id) else { return };
        for (_, controller) in conn.incoming.drain() {
            controller.abort(Some(error.clone()));
        }
        for (_, pending) in conn.calls.drain() {
            pending.signal.remove_listener(pending.listener);
            let _ = pending.reply.send(Err(error.clone()));
        }
        for (_, vote) in conn.votes.drain() {
            let _ = vote.send(Err(error.clone()));
        }
        for waiter in std::mem::take(&mut conn.sync_waiters) {
            fail_waiter(waiter, error.clone());
        }
        let relays: Vec<_> = conn.relayed_refs.values().copied().collect();
        for relay in relays {
            self.relay_close(relay, &error, false, true);
        }
        let Some(conn) = self.conns.shift_remove(&id) else { return };
        for binding in conn.remotes.into_values() {
            // A parent's link may be replaced (reconnect); a downstream host that went away is gone.
            match conn.role {
                Role::Upstream => binding.detached(),
                Role::Downstream => binding.gone(error.clone()),
            }
        }
        self.deferred_hellos.shift_remove(&id);
        let upstream = self.upstream == Some(id);
        if upstream {
            self.upstream = None;
        }
        if let Some(host) = self.host() {
            host.connection_closed(id, upstream);
        }
    }
}
