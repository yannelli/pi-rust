//! Relays: a hold from an origin link for another host, forwarded to the link a route resolves. A relay owns what
//! is done through it (calls waiting for the route, then their downstream twins); closing it undoes all of it once.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::conn::{ConnShared, Overflow, Served, unavailable};
use super::net::{ConnId, Msg, Net, RelayId, RelayedCall, Reply};
use super::route::resolve_route;
use crate::context::AbortSignal;
use crate::error::{ChordError, ChordResult};
use crate::host2::contract::{contract_from_description, resolve_host};
use crate::host2::errors::{ChordErrorCode, ErrorOptions, invalid};
use crate::host2::wire::to_wire_error;
use crate::host2::wire::{Call, CallResult, Frame, Hold, Payload, Shape, StateFrame, StateKind, from_wire_error};
use crate::json::Json;

/// What the route resolution checks from outside the task: still authorized as when the hold arrived.
pub(crate) struct RelayShared {
    closed: AtomicBool,
    generation: u64,
    origin: Arc<ConnShared>,
}

impl RelayShared {
    pub(crate) fn current(&self) -> bool {
        !self.closed.load(Ordering::SeqCst)
            && !self.origin.link.closed()
            && self.origin.generation.load(Ordering::SeqCst) == self.generation
    }
}

/// A relayed call: its reply and the abort listener on the origin call's signal.
pub(super) struct RelayCallEntry {
    pub down_id: u64,
    pub reply: Reply<Option<Payload>>,
    pub signal: AbortSignal,
    pub listener: u64,
}

pub(crate) struct Relay {
    pub origin: ConnId,
    pub hold: Box<Hold>,
    pub principal: Json,
    /// The client connection the hold comes from (forwarded downstream).
    pub tag: Option<String>,
    pub shared: Arc<RelayShared>,
    pub down: Option<ConnId>,
    pub down_id: u64,
    /// The origin wants state (forwarded as `state: false`, `sub`, `unsub`).
    pub state_wanted: bool,
    /// The members of the first unit, still to forward past the origin's window.
    urgent: Option<HashSet<String>>,
    shaped: bool,
    /// Members whose ops this relay dropped: downstream ops are ignored until their reset arrives.
    awaiting: HashSet<String>,
    pub(super) calls: HashMap<u64, RelayCallEntry>,
    /// Calls waiting for the route.
    queued: Vec<(RelayedCall, RelayCallEntry)>,
}

fn closed_error() -> ChordError {
    ChordError::code_only(ChordErrorCode::Gone, "relayed hold closed")
}

fn cancelled(reason: ChordError) -> ChordError {
    ChordError::coded(ChordErrorCode::Aborted, "call cancelled", ErrorOptions::default().with_cause(reason))
}

impl Net {
    /// A hold for another host: relaying needs a route (where authorization lives). Placement is checked now; the
    /// route resolves in a task, and the origin's authorization is checked after every step of it.
    pub(super) fn start_relay(&mut self, origin: ConnId, hold: Box<Hold>, principal: Json, tag: Option<String>) {
        let Some(conn) = self.conns.get(&origin) else { return };
        let shared = Arc::new(RelayShared {
            closed: AtomicBool::new(false),
            generation: conn.shared.generation.load(Ordering::SeqCst),
            origin: conn.shared.clone(),
        });
        let id = self.relay_id();
        let (at, origin_hold) = (hold.at.clone(), hold.id);
        let relay = Relay {
            origin,
            state_wanted: hold.state,
            hold,
            principal: principal.clone(),
            tag,
            shared: shared.clone(),
            down: None,
            down_id: 0,
            urgent: None,
            shaped: false,
            awaiting: HashSet::new(),
            calls: HashMap::new(),
            queued: Vec::new(),
        };
        let checked = self.check_relay(&relay.hold);
        self.relays.insert(id, relay);
        if let Some(conn) = self.conns.get_mut(&origin) {
            conn.served.insert(origin_hold, Served::Relay(id));
        }
        let route = match checked {
            Ok(route) => route,
            Err(error) => return self.relay_close(id, &error, true, true),
        };
        let net = self.me.clone();
        self.handle.spawn(async move {
            let current = || shared.current();
            match resolve_route(&net, &route, &at, &principal, current).await {
                Ok(down) => net.post(Msg::RelayOpen { relay: id, down }),
                Err(error) => net.post(Msg::RelayFailed { relay: id, error }),
            };
        });
    }

    /// The hold names its target; its contract token says where the type lives. They must agree.
    fn check_relay(&self, hold: &Hold) -> ChordResult<super::Route> {
        let token = contract_from_description(hold.token.clone())?;
        let placement =
            token.description().host().ok_or_else(|| invalid("token", "a local type cannot be relayed", None))?;
        if resolve_host(token.name(), placement, &hold.input)? != hold.at {
            let message = format!("{} with this input does not live on {}", token.name(), hold.at);
            return Err(invalid("placement", message, None));
        }
        let host = self.host().ok_or_else(|| unavailable("host closed"))?;
        host.route_for(&hold.at).ok_or_else(|| match self.child_named(&hold.at) {
            None => unavailable(format!("no route to {}", hold.at)),
            Some(_) => ChordError::code_only(ChordErrorCode::Forbidden, format!("no route authorizes {}", hold.at)),
        })
    }

    pub(super) fn relay_resolved(&mut self, id: RelayId, down: ConnId) {
        let Some(relay) = self.relays.get(&id) else { return };
        if !relay.shared.current() || !self.conns.get(&relay.origin).is_some_and(|conn| conn.open()) {
            return;
        }
        let Some(conn) = self.conns.get_mut(&down) else {
            return self.relay_close(id, &unavailable("relay link closed"), false, true);
        };
        let down_id = conn.next_id();
        conn.relayed_refs.insert(down_id, id);
        let Some(relay) = self.relays.get_mut(&id) else { return };
        relay.down = Some(down);
        relay.down_id = down_id;
        let hold = Hold {
            id: down_id,
            at: relay.hold.at.clone(),
            token: relay.hold.token.clone(),
            input: relay.hold.input.clone(),
            principal: Some(relay.principal.clone()),
            origin: relay.tag.clone(),
            state: relay.state_wanted,
            again: relay.hold.again,
        };
        let queued = std::mem::take(&mut relay.queued);
        self.send(down, &Frame::Hold(Box::new(hold)));
        for (call, entry) in queued {
            self.forward_call(id, call, entry);
        }
    }

    /// The origin starts or stops the hold's state.
    pub(super) fn relay_want_state(&mut self, id: RelayId, wanted: bool) {
        let Some(relay) = self.relays.get_mut(&id).filter(|relay| relay.state_wanted != wanted) else { return };
        relay.state_wanted = wanted;
        let (origin, origin_hold, down, down_id) = (relay.origin, relay.hold.id, relay.down, relay.down_id);
        if !wanted {
            relay.awaiting.clear();
            self.drop_hold_state(origin, origin_hold);
        }
        if let Some(down) = down {
            self.send(down, &if wanted { Frame::Sub { hold: down_id } } else { Frame::Unsub { hold: down_id } });
        }
    }

    /// The origin dropped what waited for member `m` (or asked for it again): ask downstream for a reset, and
    /// ignore its ops until the reset arrives.
    pub(super) fn relay_resync(&mut self, id: RelayId, m: &str) {
        let Some(relay) = self.relays.get_mut(&id) else { return };
        relay.awaiting.insert(m.to_string());
        if let Some(down) = relay.down {
            let frame = Frame::Resync { hold: relay.down_id, m: m.to_string() };
            self.send(down, &frame);
        }
    }

    /// A shape from downstream: forwarded at once; the members of the first unit then skip the origin's window.
    pub(super) fn relay_shape(&mut self, id: RelayId, frame: Shape) {
        let Some(relay) = self.relays.get_mut(&id) else { return };
        if !relay.shaped {
            relay.shaped = true;
            if !relay.hold.again {
                relay.urgent = Some(frame.state.iter().cloned().collect());
            }
        }
        let (origin, hold) = (relay.origin, relay.hold.id);
        self.send(origin, &Frame::Shape(Shape { id: hold, ..frame }));
    }

    pub(super) fn relay_state(&mut self, id: RelayId, frame: StateFrame) {
        let Some(relay) = self.relays.get_mut(&id) else { return };
        if relay.awaiting.contains(&frame.m) {
            if frame.kind == StateKind::Ops {
                return;
            }
            relay.awaiting.remove(&frame.m);
        }
        let urgent = relay.urgent.as_mut().is_some_and(|urgent| urgent.remove(&frame.m));
        let (origin, hold) = (relay.origin, relay.hold.id);
        let out = StateFrame { hold, ..frame };
        if urgent {
            return self.send(origin, &Frame::State(out));
        }
        self.send_state(origin, out, Overflow::Resync(id));
    }

    /// Closes a relay. `release_down`: the downstream hold is still there (origin released, disconnected, or was
    /// revoked): cancel its calls and release it. `notify`: tell the origin the hold is gone.
    pub(super) fn relay_close(&mut self, id: RelayId, error: &ChordError, release_down: bool, notify: bool) {
        let Some(relay) = self.relays.remove(&id) else { return };
        relay.shared.closed.store(true, Ordering::SeqCst);
        for (_, entry) in relay.calls {
            if release_down && let Some(down) = relay.down {
                self.send(down, &Frame::Cancel { id: entry.down_id });
            }
            if let Some(down) = relay.down.and_then(|down| self.conns.get_mut(&down)) {
                down.relayed_calls.remove(&entry.down_id);
            }
            finish(entry, Err(error.clone()));
        }
        for (_, entry) in relay.queued {
            finish(entry, Err(error.clone()));
        }
        let origin_hold = relay.hold.id;
        self.drop_hold_state(relay.origin, origin_hold);
        if let Some(conn) = self.conns.get_mut(&relay.origin)
            && matches!(conn.served.get(&origin_hold), Some(Served::Relay(served)) if *served == id)
        {
            conn.served.shift_remove(&origin_hold);
        }
        if let Some(down) = relay.down.and_then(|down| self.conns.get_mut(&down)) {
            down.relayed_refs.remove(&relay.down_id);
            if release_down {
                down.send(&Frame::Release { id: relay.down_id });
            }
        }
        if notify {
            self.send(relay.origin, &Frame::Gone { id: origin_hold, error: to_wire_error(error) });
        }
    }

    /// A call through the relay: forwarded once the route resolved, with the root it runs in.
    pub(super) fn relay_call(&mut self, id: RelayId, call: RelayedCall, reply: Reply<Option<Payload>>) {
        let Some(relay) = self.relays.get_mut(&id) else {
            let _ = reply.send(Err(closed_error()));
            return;
        };
        if let Some(reason) = call.signal.reason() {
            let _ = reply.send(Err(cancelled(reason)));
            return;
        }
        let me = self.me.clone();
        let origin_call = call.id;
        let listener = call.signal.add_listener(move |reason| {
            me.post(Msg::RelayCancel { relay: id, call: origin_call, error: cancelled(reason.clone()) });
        });
        let entry = RelayCallEntry { down_id: 0, reply, signal: call.signal.clone(), listener };
        if relay.down.is_none() {
            relay.queued.push((call, entry));
            return;
        }
        self.forward_call(id, call, entry);
    }

    fn forward_call(&mut self, id: RelayId, call: RelayedCall, mut entry: RelayCallEntry) {
        let Some(relay) = self.relays.get_mut(&id) else { return finish(entry, Err(closed_error())) };
        let (Some(down), hold) = (relay.down, relay.down_id) else { return finish(entry, Err(closed_error())) };
        let Some(conn) = self.conns.get_mut(&down) else { return finish(entry, Err(closed_error())) };
        let down_id = conn.next_id();
        entry.down_id = down_id;
        conn.relayed_calls.insert(down_id, (id, call.id));
        relay.calls.insert(call.id, entry);
        if let Some(host) = self.host() {
            host.root_sent(&call.root.root_id, down);
        }
        let frame = Call { id: down_id, hold, member: call.member, args: call.args, root: Some(call.root) };
        self.send(down, &Frame::Call(frame));
    }

    /// The origin cancelled a relayed call (or its root aborted): cancel downstream and settle it.
    pub(super) fn relay_cancel(&mut self, id: RelayId, call: u64, error: ChordError) {
        let Some(relay) = self.relays.get_mut(&id) else { return };
        if let Some(index) = relay.queued.iter().position(|(queued, _)| queued.id == call) {
            let (_, entry) = relay.queued.remove(index);
            return finish(entry, Err(error));
        }
        let Some(entry) = relay.calls.remove(&call) else { return };
        if let Some(conn) = relay.down.and_then(|down| self.conns.get_mut(&down)) {
            conn.relayed_calls.remove(&entry.down_id);
            conn.send(&Frame::Cancel { id: entry.down_id });
        }
        finish(entry, Err(error));
    }

    pub(super) fn relay_finished(&mut self, id: RelayId, call: u64, result: CallResult) {
        let Some(relay) = self.relays.get_mut(&id) else { return };
        let Some(entry) = relay.calls.remove(&call) else { return };
        if let Some(conn) = relay.down.and_then(|down| self.conns.get_mut(&down)) {
            conn.relayed_calls.remove(&entry.down_id);
        }
        let outcome = match result.error {
            Some(error) => Err(from_wire_error(&error)),
            None => Ok(result.value),
        };
        finish(entry, outcome);
    }

    /// Revokes the identity of a link: relays made under it close at once (also those still resolving).
    pub(super) fn revoke(&mut self, id: ConnId, next: Option<Json>) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        conn.shared.generation.fetch_add(1, Ordering::SeqCst);
        let relays: Vec<RelayId> = conn
            .served
            .values()
            .filter_map(|served| match served {
                Served::Relay(relay) => Some(*relay),
                Served::Local(_) => None,
            })
            .collect();
        let error = ChordError::code_only(ChordErrorCode::Gone, "principal invalidated");
        for relay in relays {
            self.relay_close(relay, &error, true, true);
        }
        match next {
            None => self.close_link(id, "principal invalidated"),
            Some(next) => {
                if let Some(conn) = self.conns.get_mut(&id) {
                    conn.principal = next;
                }
            }
        }
    }
}

fn finish(entry: RelayCallEntry, outcome: ChordResult<Option<Payload>>) {
    entry.signal.remove_listener(entry.listener);
    let _ = entry.reply.send(outcome);
}
