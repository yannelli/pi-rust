//! Calls the peer makes on holds it has here: each runs in a root (the caller's, from a host, or a new one), on
//! the served object or through a relay, and is answered with a `result`.

use std::sync::Arc;

use super::conn::Served;
use super::net::{ConnId, Msg, Net, RelayedCall};
use super::route::relay_call;
use crate::context::AbortController;
use crate::error::{ChordError, ChordResult};
use crate::host2::errors::ChordErrorCode;
use crate::host2::exec::spawn_now;
use crate::host2::link::unpack;
use crate::host2::object::{Ctx, HostCore, RefImpl, RootOptions};
use crate::host2::wire::{Call, CallResult, Frame, Payload, PeerKind, RootRef, to_wire_error};
use crate::json::Json;
use crate::sync::BoxFuture;

type Run = Box<dyn FnOnce(Ctx) -> BoxFuture<ChordResult<Option<Payload>>> + Send>;

enum Target {
    Local(Arc<RefImpl>),
    Relay(super::net::RelayId),
}

impl Net {
    pub(super) fn on_call(&mut self, id: ConnId, call: Call) {
        let (Some(_), Some(conn)) = (self.host(), self.conns.get_mut(&id)) else { return };
        if conn.incoming.contains_key(&call.id) {
            return self.violation(id, &format!("duplicate call id {}", call.id));
        }
        let refuse = |error: ChordError| {
            Frame::Result(CallResult { id: call.id, value: None, error: Some(to_wire_error(&error)) })
        };
        if conn.incoming.len() >= conn.max_calls {
            let message = format!(
                "Too many calls in progress on this connection ({}); wait for earlier ones to finish",
                conn.max_calls
            );
            let error = ChordError::with_reason(ChordErrorCode::Unavailable, "too_many_calls", message);
            return conn.send(&refuse(error));
        }
        let (target, principal) = match conn.served.get(&call.hold) {
            Some(Served::Local(served)) => (Target::Local(served.reference.clone()), served.principal.clone()),
            Some(Served::Relay(relay)) => match self.relays.get(relay) {
                Some(entry) => (Target::Relay(*relay), entry.principal.clone()),
                None => return conn.send(&refuse(ChordError::code_only(ChordErrorCode::Gone, "unknown hold"))),
            },
            None => return conn.send(&refuse(ChordError::code_only(ChordErrorCode::Gone, "unknown hold"))),
        };
        let controller = AbortController::new();
        conn.incoming.insert(call.id, controller.clone());
        if let Some(Served::Local(served)) = conn.served.get_mut(&call.hold)
            && served.shape_seq == 0
        {
            served.held.push((call, controller));
            return;
        }
        self.start_call(id, call, controller, target, principal);
    }

    /// Starts the calls a hold kept until its first unit (or its end: they fail on the ended reference).
    pub(super) fn start_held(
        &mut self,
        id: ConnId,
        reference: &Arc<RefImpl>,
        principal: &Json,
        held: Vec<(Call, AbortController)>,
    ) {
        for (call, controller) in held {
            self.start_call(id, call, controller, Target::Local(reference.clone()), principal.clone());
        }
    }

    fn start_call(&mut self, id: ConnId, call: Call, controller: AbortController, target: Target, principal: Json) {
        let (Some(host), Some(conn)) = (self.host(), self.conns.get(&id)) else { return };
        // Roots are honored from hosts only; a request without one (or from a client) starts one here.
        let from_host = conn.peer.kind == PeerKind::Host;
        let root = if from_host { self.admit_root(id, call.root) } else { None };
        let options = RootOptions { principal: Some(principal), signal: Some(controller.signal()), code_version: None };
        let (call_id, member, args, net) = (call.id, call.member, call.args, self.me.clone());
        let run: Run = match target {
            Target::Local(reference) => Box::new(move |ctx| {
                Box::pin(async move {
                    let args = args.as_ref().map(unpack).transpose()?;
                    let object = reference.object().clone();
                    Ok(object.call(reference, member, args, ctx).await?.map(Payload::Value))
                })
            }),
            Target::Relay(relay) => Box::new(move |ctx| {
                let root = RootRef { root_id: ctx.root_id().to_string(), code_version: ctx.code_version() as u64 };
                // The root's signal: a cancel from the origin and the root's code version retiring both reach it.
                let call = RelayedCall { id: call_id, member, args, root, signal: ctx.signal().clone() };
                relay_call(net, relay, call)
            }),
        };
        let future = match root {
            None => host.root_with(options, run),
            Some(root) => host.join_with(root.root_id, root.code_version as i64, options, Some(id), run),
        };
        let me = self.me.clone();
        spawn_now(Some(&self.handle), async move {
            let result = future.await;
            me.post(Msg::CallDone { conn: id, id: call_id, result });
        });
    }

    pub(super) fn call_done(&mut self, id: ConnId, call: u64, result: ChordResult<Option<Payload>>) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        let frame = match result {
            Ok(value) => CallResult { id: call, value, error: None },
            Err(error) => CallResult { id: call, value: None, error: Some(to_wire_error(&error)) },
        };
        conn.send(&Frame::Result(frame));
        conn.incoming.remove(&call);
    }

    /// A child that connected behind this host runs roots of a code version this host never had (fenced). Such a
    /// root is admitted in this host's current code version at its first call here, and keeps it for its later
    /// calls (`superseded` once this host retired it). Other unknown code versions stay `superseded`.
    fn admit_root(&mut self, id: ConnId, root: Option<RootRef>) -> Option<RootRef> {
        let host = self.host()?;
        let conn = self.conns.get_mut(&id)?;
        let root = root?;
        if !conn.downstream() || !conn.fenced.contains(&root.code_version) {
            return Some(root);
        }
        if host.has_code_version(root.code_version as i64) {
            return Some(root);
        }
        let current = host.current_code_version();
        let code_version = *conn.admitted.entry(root.root_id.clone()).or_insert(current);
        Some(RootRef { root_id: root.root_id, code_version: code_version as u64 })
    }
}
