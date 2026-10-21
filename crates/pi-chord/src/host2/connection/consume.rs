//! The consumer side: holds this host's remote references make over a link, and their calls.

use std::sync::Arc;

use super::conn::{PendingCall, SyncWaiter, unavailable};
use super::net::{ConnId, Msg, Net, OutCall, Reply};
use super::route::resolve_route;
use crate::error::ChordError;
use crate::host2::errors::ChordErrorCode;
use crate::host2::exec::call_aborted;
use crate::host2::object::HostCore;
use crate::host2::remote::RemoteBinding;
use crate::host2::wire::{Call, Frame, Hold, Payload, PeerKind};
use crate::json::Json;

impl Net {
    /// A downstream host connection by its configured name (never by what a peer announced).
    pub(super) fn child_named(&self, name: &str) -> Option<ConnId> {
        let named =
            |conn: &&super::conn::Conn| conn.open() && conn.child_host() && conn.peer.name.as_deref() == Some(name);
        self.conns.values().find(named).map(|conn| conn.id)
    }

    /// Holds a new binding over the link to its host: the parent's, a child's, or one a route resolves.
    pub(super) fn hold_binding(&mut self, binding: Arc<RemoteBinding>) {
        if binding.ended() || binding.hold().is_some() {
            return;
        }
        if let Some(upstream) = self.upstream {
            return self.hold_on(upstream, &binding, false);
        }
        let at = binding.at.clone();
        if let Some(child) = self.child_named(&at) {
            return self.when_synced(child, SyncWaiter::Hold(binding));
        }
        let Some(host) = self.host() else { return };
        let Some(route) = host.route_for(&at) else {
            return binding.gone(unavailable(format!("no route to {at}")));
        };
        let principal: Json = binding.principal.clone().unwrap_or_else(|| host.principal().clone());
        let net = self.me.clone();
        self.handle.spawn(async move {
            match resolve_route(&net, &route, &at, &principal, || true).await {
                Ok(conn) => {
                    net.post(Msg::HoldOn { conn, binding });
                }
                Err(error) => binding.gone(error),
            }
        });
    }

    /// Sends a hold for `binding`; `again`: the binding already has values (a re-hold), its unit is not urgent.
    pub(super) fn hold_on(&mut self, id: ConnId, binding: &Arc<RemoteBinding>, again: bool) {
        let Some(conn) = self.conns.get_mut(&id).filter(|conn| !conn.closing) else {
            return binding.gone(unavailable("link closed before the hold"));
        };
        let hold = conn.next_id();
        let Some(state) = binding.bound(id, hold) else { return };
        conn.remotes.insert(hold, binding.clone());
        conn.send(&Frame::Hold(Box::new(Hold {
            id: hold,
            at: binding.at.clone(),
            token: binding.contract.description().clone(),
            input: binding.input.clone(),
            principal: binding.principal.clone(),
            origin: None,
            state,
            again,
        })));
    }

    pub(super) fn release_remote(&mut self, id: ConnId, hold: u64) {
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.remotes.remove(&hold);
            conn.send(&Frame::Release { id: hold });
        }
    }

    /// Starts (`sub`) or stops (`unsub`) the state of a hold.
    pub(super) fn want_remote_state(&mut self, id: ConnId, hold: u64, wanted: bool) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        if conn.remotes.contains_key(&hold) {
            conn.send(&if wanted { Frame::Sub { hold } } else { Frame::Unsub { hold } });
        }
    }

    pub(super) fn call(&mut self, id: ConnId, call: OutCall, reply: Reply<Option<Json>>) {
        let (Some(host), Some(conn)) = (self.host(), self.conns.get_mut(&id).filter(|conn| !conn.closing)) else {
            let _ = reply.send(Err(unavailable("link closed")));
            return;
        };
        let call_id = conn.next_id();
        if call.signal.aborted() {
            let _ = reply.send(Err(ChordError::code_only(ChordErrorCode::Aborted, "call aborted")));
            return;
        }
        let me = self.me.clone();
        let listener = call.signal.add_listener(move |reason| {
            me.post(Msg::CallAborted { conn: id, id: call_id, reason: reason.clone() });
        });
        conn.calls.insert(call_id, PendingCall { reply, signal: call.signal, listener });
        // Clients never carry roots or code versions: the server starts roots for them.
        let root = call.root.filter(|_| host.kind() == PeerKind::Host);
        if let Some(root) = &root {
            host.root_sent(&root.root_id, id);
        }
        let args = call.args.map(Payload::Value);
        conn.send(&Frame::Call(Call { id: call_id, hold: call.hold, member: call.member, args, root }));
    }

    pub(super) fn call_aborted(&mut self, id: ConnId, call: u64, reason: ChordError) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        if let Some(pending) = conn.calls.remove(&call) {
            conn.send(&Frame::Cancel { id: call });
            let _ = pending.reply.send(Err(call_aborted(reason)));
        }
    }
}
