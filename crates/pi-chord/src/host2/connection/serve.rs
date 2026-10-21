//! The provider side: holds the peer made on this host. A hold implies the state of its variant (unless it says
//! `state: false`): the answer is a unit, the shape and then a snapshot of each state member the consumer lacks,
//! followed by the members' ops and resets. A reload or variant change sends a unit with the members that are new.

use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use super::Role;
use super::conn::{Feed, Overflow, Served, ServedLocal};
use super::flow::unit_key;
use super::net::{ConnId, Msg, Net};
use crate::delta::encoder;
use crate::error::{ChordError, ChordResult};
use crate::host2::contract::{LOCAL_VARIANT, MemberDescription, Scope, contract_from_description, resolve_host};
use crate::host2::errors::{ChordErrorCode, ErrorOptions, invalid};
use crate::host2::object::{HostCore, RefCore, RefImpl};
use crate::host2::schema::assert_valid;
use crate::host2::types::{GetOptions, RefStatus, Shape, StateEvent};
use crate::host2::wire::{Frame, Hold, Payload, StateFrame, StateKind, to_wire_error};
use crate::json::Json;
use crate::sync::lock;

/// The served hold `hold`, if it is still the one numbered `seq`.
fn local(net: &mut Net, id: ConnId, hold: u64, seq: Option<u64>) -> Option<&mut ServedLocal> {
    match net.conns.get_mut(&id)?.served.get_mut(&hold)? {
        Served::Local(served) if seq.is_none_or(|seq| served.seq == seq) => Some(served),
        _ => None,
    }
}

/// The state members of `shape` the consumer's contract knows.
fn state_members(reference: &RefImpl, shape: &Shape) -> Vec<String> {
    let is_state = |name: &&String| {
        matches!(reference.describe(shape.variant.as_deref(), name), Some(MemberDescription::State { .. }))
    };
    shape.members.iter().filter(is_state).cloned().collect()
}

/// Clients (browsers) holding an outdated contract are told to reload their code.
fn client_error(error: ChordError) -> ChordError {
    if error.code() == Some(ChordErrorCode::ContractMismatch) {
        let message = error.to_string();
        return ChordError::coded(ChordErrorCode::UpgradeRequired, message, ErrorOptions::default().with_cause(error));
    }
    error
}

impl Net {
    /// Identity stamping: a downstream link acts with the identity it was accepted with, whatever the frame says.
    /// Only the parent may forward one (relayed requests).
    fn principal_of(&self, id: ConnId, forwarded: Option<Json>) -> Json {
        let conn = &self.conns[&id];
        match conn.role {
            Role::Downstream => conn.principal.clone(),
            Role::Upstream => forwarded.unwrap_or_else(|| conn.principal.clone()),
        }
    }

    pub(super) fn on_hold(&mut self, id: ConnId, hold: Box<Hold>) {
        let (Some(host), Some(conn)) = (self.host(), self.conns.get(&id)) else { return };
        if conn.served.contains_key(&hold.id) {
            return self.violation(id, &format!("duplicate hold id {}", hold.id));
        }
        let origin = if conn.downstream() { conn.origin_id.clone() } else { hold.origin.clone() };
        let role = conn.role;
        let principal = self.principal_of(id, hold.principal.clone());
        let client = origin.is_some();
        if hold.at != host.name() {
            if role == Role::Upstream {
                let error = super::conn::unavailable(format!("{} does not relay", host.name()));
                return self.send(id, &Frame::Gone { id: hold.id, error: to_wire_error(&error) });
            }
            return self.start_relay(id, hold, principal, origin);
        }
        let acquired = (|| {
            let token = contract_from_description(hold.token.clone())?;
            if token.scope() != Scope::Remote {
                return Err(invalid(
                    "token",
                    format!("{} is local; only remote types cross links", token.name()),
                    None,
                ));
            }
            let placement = token.description().host().expect("a remote token has a placement");
            if resolve_host(token.name(), placement, &hold.input)? != host.name() {
                let message = format!("{} with this input does not live on {}", token.name(), host.name());
                return Err(invalid("placement", message, None));
            }
            let options = GetOptions { principal: Some(principal.clone()), ..GetOptions::default() };
            host.acquire_served(&token, &hold.input, &options, client)
        })();
        let reference = match acquired {
            Ok(reference) => reference,
            Err(error) => {
                let error = if client { client_error(error) } else { error };
                return self.send(id, &Frame::Gone { id: hold.id, error: to_wire_error(&error) });
            }
        };
        let (seq, hold_id) = (self.seq(), hold.id);
        let (me, weak) = (self.me.clone(), Arc::downgrade(&reference));
        let on_shape = reference.on_shape(Arc::new(move |_| {
            let shape_seq = weak.upgrade().map_or(0, |reference| reference.shape_seq());
            me.post(Msg::Unit { conn: id, hold: hold_id, seq, shape_seq });
        }));
        let me = self.me.clone();
        let on_gone = reference.on_gone(move |error| {
            me.post(Msg::ServedGone { conn: id, hold: hold_id, seq, error: error.clone() });
        });
        let served = ServedLocal {
            seq,
            reference: reference.clone(),
            principal,
            origin,
            wants_state: hold.state,
            again: hold.again,
            unit_sent: false,
            shape_seq: 0,
            feeds: IndexMap::new(),
            listeners: vec![on_shape, on_gone],
            held: Vec::new(),
        };
        let Some(conn) = self.conns.get_mut(&id) else { return };
        conn.served.insert(hold.id, Served::Local(Box::new(served)));
        if reference.status() == RefStatus::Gone {
            let error = reference.error().unwrap_or_else(|| ChordError::code_only(ChordErrorCode::Gone, "gone"));
            return self.served_gone(id, hold.id, seq, &error);
        }
        if let Some(shape_seq) = reference.live_seq() {
            self.shape_unit(id, hold.id, seq, shape_seq);
        }
    }

    /// A shape change (live, reload, variant) not answered yet gets a unit.
    pub(super) fn shape_unit(&mut self, id: ConnId, hold: u64, seq: u64, shape_seq: u64) {
        let Some(served) = local(self, id, hold, Some(seq)) else { return };
        if served.shape_seq >= shape_seq {
            return;
        }
        served.shape_seq = shape_seq;
        self.unit(id, hold);
        let Some(served) = local(self, id, hold, None) else { return };
        let (held, reference, principal) =
            (std::mem::take(&mut served.held), served.reference.clone(), served.principal.clone());
        self.start_held(id, &reference, &principal, held);
    }

    /// Members that left the shape lose their stream, and a unit goes out with the shape and the members the
    /// consumer lacks. The first unit of a hold (not a re-hold) is urgent.
    pub(super) fn unit(&mut self, id: ConnId, hold: u64) {
        let Some(served) = local(self, id, hold, None) else { return };
        let Some(shape) = served.reference.shape() else { return };
        let members = if served.wants_state { state_members(&served.reference, &shape) } else { Vec::new() };
        let gone: Vec<String> = served.feeds.keys().filter(|name| !members.contains(name)).cloned().collect();
        for name in &gone {
            if let Some(feed) = served.feeds.shift_remove(name) {
                feed.unsubscribe.unsubscribe();
            }
        }
        let urgent = served.feeds.is_empty() && !(served.again && !served.unit_sent);
        for name in gone {
            self.drop_state(id, hold, &name);
        }
        if urgent { self.send_unit(id, hold) } else { self.queue_unit(id, hold) }
    }

    /// Builds and sends a unit from the current shape and values.
    pub(super) fn send_unit(&mut self, id: ConnId, hold: u64) {
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.pending.shift_remove(&unit_key(hold));
        }
        let Some(served) = local(self, id, hold, None) else { return };
        let Some(shape) = served.reference.shape() else { return };
        served.unit_sent = true;
        let members = if served.wants_state { state_members(&served.reference, &shape) } else { Vec::new() };
        let listed: Vec<String> = members.into_iter().filter(|name| !served.feeds.contains_key(name)).collect();
        let mut snaps = Vec::new();
        for name in listed {
            if let Some(frame) = self.attach_member(id, hold, &name, StateKind::Snap) {
                snaps.push(frame);
            }
        }
        let state = snaps.iter().map(|frame| frame.m.clone()).collect();
        let frame = crate::host2::wire::Shape {
            id: hold,
            variant: shape.variant,
            members: shape.members,
            contract: shape.contract,
            state,
        };
        self.send(id, &Frame::Shape(frame));
        for frame in snaps {
            self.send(id, &Frame::State(frame));
        }
    }

    /// Subscribes member `name` for the consumer: its snapshot as a frame of `kind`, and a feed whose later
    /// events arrive as messages (after the snapshot is sent). `None` if the member is not readable state.
    fn attach_member(&mut self, id: ConnId, hold: u64, name: &str, kind: StateKind) -> Option<StateFrame> {
        let feed_id = self.seq();
        let me = self.me.clone();
        let served = local(self, id, hold, None)?;
        let readable = RefCore::state(&*served.reference, name)?;
        // The first event (the snapshot, normally delivered at once) is the unit's; later ones are messages.
        let first: Arc<Mutex<Option<Option<Json>>>> = Arc::default();
        let (seen, member) = (first.clone(), name.to_string());
        let unsubscribe = readable.events(Arc::new(move |event: &StateEvent| {
            let mut first = lock(&seen);
            if first.is_none() {
                *first = Some(Some(event.value().clone()));
                return;
            }
            drop(first);
            let event = event.clone();
            me.post(Msg::Feed { conn: id, hold, name: member.clone(), feed: feed_id, event });
        }));
        let value = lock(&first).clone().flatten();
        served.feeds.insert(name.to_string(), Feed { id: feed_id, unsubscribe, encoder: encoder() });
        Some(snapshot(&served.reference, hold, name, kind, value))
    }

    /// A later event of a member's stream: ops are encoded, a reset starts a new stream.
    pub(super) fn feed_event(&mut self, id: ConnId, hold: u64, name: &str, feed_id: u64, event: &StateEvent) {
        let Some(served) = local(self, id, hold, None) else { return };
        let Some(feed) = served.feeds.get_mut(name).filter(|feed| feed.id == feed_id) else { return };
        let frame = match event {
            StateEvent::Ops { ops, .. } => {
                let wire = feed.encoder.encode(ops);
                let ops = Some(wire.iter().map(|op| op.to_json().as_array().cloned().unwrap_or_default()).collect());
                StateFrame { hold, m: name.to_string(), kind: StateKind::Ops, value: None, ops, error: None }
            }
            _ => {
                feed.encoder = encoder();
                snapshot(&served.reference, hold, name, StateKind::Reset, Some(event.value().clone()))
            }
        };
        self.send_state(id, frame, Overflow::Resend);
    }

    /// Sends member `name` again, now, as a reset from its current value (a lazy slot is flushed, or `resync`).
    pub(super) fn resend(&mut self, id: ConnId, hold: u64, name: &str) {
        let Some(served) = local(self, id, hold, None) else { return };
        let Some(feed) = served.feeds.shift_remove(name) else { return };
        feed.unsubscribe.unsubscribe();
        if let Some(frame) = self.attach_member(id, hold, name, StateKind::Reset) {
            self.send(id, &Frame::State(frame));
        }
    }

    /// The consumer starts (`sub`) or stops (`unsub`) the state of a hold.
    pub(super) fn on_want_state(&mut self, id: ConnId, hold: u64, wanted: bool) {
        let Some(served) = self.conns.get_mut(&id).and_then(|conn| conn.served.get_mut(&hold)) else { return };
        let served = match served {
            Served::Relay(relay) => {
                let relay = *relay;
                return self.relay_want_state(relay, wanted);
            }
            Served::Local(served) => served,
        };
        if served.wants_state == wanted {
            return;
        }
        served.wants_state = wanted;
        if wanted {
            return self.unit(id, hold);
        }
        for (_, feed) in served.feeds.drain(..) {
            feed.unsubscribe.unsubscribe();
        }
        self.drop_hold_state(id, hold);
    }

    /// The consumer (a relay that dropped ops) asks for member `m` again as a reset.
    pub(super) fn on_resync(&mut self, id: ConnId, hold: u64, m: &str) {
        let Some(served) = self.conns.get(&id).and_then(|conn| conn.served.get(&hold)) else { return };
        let relay = match served {
            Served::Relay(relay) => Some(*relay),
            Served::Local(served) if served.feeds.contains_key(m) => None,
            Served::Local(_) => return,
        };
        match relay {
            Some(relay) => self.relay_resync(relay, m),
            None => self.reset_later(id, hold, m),
        }
    }

    /// Forgets a served hold: unsubscribes its feeds and releases it (locally or downstream).
    pub(super) fn drop_served(&mut self, id: ConnId, hold: u64, release: bool) {
        let Some(served) = self.conns.get_mut(&id).and_then(|conn| conn.served.shift_remove(&hold)) else { return };
        let served = match served {
            Served::Relay(relay) => {
                let error = ChordError::code_only(ChordErrorCode::Released, "relayed hold released");
                return self.relay_close(relay, &error, release, false);
            }
            Served::Local(served) => served,
        };
        self.start_held(id, &served.reference, &served.principal, served.held);
        for (_, feed) in served.feeds {
            feed.unsubscribe.unsubscribe();
        }
        for listener in served.listeners {
            listener.unsubscribe();
        }
        self.drop_hold_state(id, hold);
        if release {
            served.reference.release();
        }
    }

    pub(super) fn served_gone(&mut self, id: ConnId, hold: u64, seq: u64, error: &ChordError) {
        if local(self, id, hold, Some(seq)).is_none() {
            return;
        }
        self.drop_served(id, hold, false);
        self.send(id, &Frame::Gone { id: hold, error: to_wire_error(error) });
    }
}

/// A member's state frame for a snapshot or reset, validated against the schema the consumer is promised.
fn snapshot(reference: &RefImpl, hold: u64, name: &str, kind: StateKind, value: Option<Json>) -> StateFrame {
    let frame = StateFrame { hold, m: name.to_string(), kind, value: None, ops: None, error: None };
    if let Err(error) = validate_snapshot(reference, name, value.as_ref()) {
        return StateFrame { kind: StateKind::Error, error: Some(to_wire_error(&error)), ..frame };
    }
    StateFrame { value: value.map(Payload::Value), ..frame }
}

/// The schema is the provider's own for a direct binding, the consumer's contract for one through an `accepts`
/// adapter (the adapter's view is what the consumer gets).
fn validate_snapshot(reference: &RefImpl, member: &str, value: Option<&Json>) -> ChordResult<()> {
    let (Some(value), Some(installed)) = (value, reference.object().current_installed()) else { return Ok(()) };
    let view = reference.current_view();
    let description = match &view {
        Some(view) if !Arc::ptr_eq(view, &installed.implementation) => {
            let variant = view.variant_name().unwrap_or(LOCAL_VARIANT);
            reference.contract().description().variant(variant).and_then(|members| members.get(member)).cloned()
        }
        _ => {
            let variant = installed.implementation.variant_name().unwrap_or(LOCAL_VARIANT);
            let members = installed.service.contract.description().variant(variant);
            members.and_then(|members| members.get(member)).cloned()
        }
    };
    if let Some(MemberDescription::State { schema }) = description {
        assert_valid("state", &format!("{}.{member}", reference.address()), &schema, Some(value))?;
    }
    Ok(())
}
