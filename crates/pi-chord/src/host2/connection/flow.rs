//! Flow control: the peer acknowledges what it processed (`ack`), and while unacknowledged frames exceed the
//! window, state waits in slots (one per stream, one per hold's unit) flushed in order as acks come. The first unit
//! of a hold is urgent; waiting ops are capped, beyond which the oldest streams become resets (`resync` on relays).

use indexmap::IndexMap;

use super::conn::{Overflow, Slot, StateSlot};
use super::net::{ConnId, Net, RelayId};
use crate::host2::wire::{Frame, StateFrame, StateKind};
use crate::json::{Json, utf16_len};

pub(super) fn state_key(hold: u64, m: &str) -> String {
    format!("s{hold}\u{0}{m}")
}

pub(super) fn unit_key(hold: u64) -> String {
    format!("u{hold}")
}

fn ops_size(ops: &[Vec<Json>]) -> usize {
    let json = Json::array(ops.iter().map(|op| Json::array(op.clone())).collect());
    utf16_len(&json.to_string())
}

fn lazy_slot(hold: u64, m: &str, lazy: bool, overflow: Overflow) -> Slot {
    Slot::State(Box::new(StateSlot { hold, m: m.to_string(), base: None, ops: Vec::new(), size: 0, lazy, overflow }))
}

impl Net {
    /// Sends a state frame, or keeps it waiting.
    pub(super) fn send_state(&mut self, id: ConnId, frame: StateFrame, overflow: Overflow) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        let key = state_key(frame.hold, &frame.m);
        if !conn.pending.contains_key(&key) {
            if !conn.full() {
                conn.send(&Frame::State(frame));
                return;
            }
            conn.pending.insert(key.clone(), lazy_slot(frame.hold, &frame.m, false, overflow));
        }
        let Some(Slot::State(slot)) = conn.pending.get_mut(&key) else { return };
        if frame.kind != StateKind::Ops {
            conn.pending_bytes -= slot.size;
            slot.base = Some(frame);
            slot.ops.clear();
            slot.size = 0;
            slot.lazy = false;
            return;
        }
        // A reset built at flush already covers these ops.
        if slot.lazy {
            return;
        }
        let ops = frame.ops.unwrap_or_default();
        let size = ops_size(&ops);
        let mut resyncs = Vec::new();
        if conn.pending_bytes + size > conn.pending_cap {
            resyncs = self.make_room(id, size, &key);
        }
        if let Some(conn) = self.conns.get_mut(&id)
            && let Some(Slot::State(slot)) = conn.pending.get_mut(&key)
            && !slot.lazy
        {
            slot.ops.extend(ops);
            slot.size += size;
            conn.pending_bytes += size;
        }
        for (relay, m) in resyncs {
            self.relay_resync(relay, &m);
        }
    }

    /// Replaces the oldest waiting streams by resets until `size` more bytes fit; `current` last, if needed.
    /// Returns the relays that must ask downstream for a reset.
    fn make_room(&mut self, id: ConnId, size: usize, current: &str) -> Vec<(RelayId, String)> {
        let Some(conn) = self.conns.get_mut(&id) else { return Vec::new() };
        let mut resyncs = Vec::new();
        let keys: Vec<String> = conn.pending.keys().cloned().collect();
        for key in keys {
            if conn.pending_bytes + size <= conn.pending_cap {
                return resyncs;
            }
            let droppable = matches!(conn.pending.get(&key), Some(Slot::State(slot)) if slot.size > 0);
            if key != current && droppable {
                resyncs.extend(drop_slot(&mut conn.pending, &mut conn.pending_bytes, &key));
            }
        }
        if conn.pending_bytes + size > conn.pending_cap {
            resyncs.extend(drop_slot(&mut conn.pending, &mut conn.pending_bytes, current));
        }
        resyncs
    }

    /// Sends member `m` of a served hold again as a reset, through the window (`resync`).
    pub(super) fn reset_later(&mut self, id: ConnId, hold: u64, m: &str) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        let key = state_key(hold, m);
        match conn.pending.get_mut(&key) {
            Some(Slot::State(slot)) => {
                conn.pending_bytes -= slot.size;
                slot.base = None;
                slot.ops.clear();
                slot.size = 0;
                slot.lazy = true;
            }
            Some(Slot::Unit { .. }) => return,
            None => {
                conn.pending.insert(key, lazy_slot(hold, m, true, Overflow::Resend));
            }
        }
        self.flush(id);
    }

    /// Queues a hold's lazy unit (a waiting one already covers this one), and sends what the window allows.
    pub(super) fn queue_unit(&mut self, id: ConnId, hold: u64) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        let key = unit_key(hold);
        if !conn.pending.contains_key(&key) {
            conn.pending.insert(key, Slot::Unit { hold });
        }
        self.flush(id);
    }

    /// Forgets what waits for a stream that ended.
    pub(super) fn drop_state(&mut self, id: ConnId, hold: u64, m: &str) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        if let Some(Slot::State(slot)) = conn.pending.shift_remove(&state_key(hold, m)) {
            conn.pending_bytes -= slot.size;
        }
    }

    /// Forgets everything that waits for a hold (released, closed, or its state stopped).
    pub(super) fn drop_hold_state(&mut self, id: ConnId, hold: u64) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        let bytes = &mut conn.pending_bytes;
        conn.pending.retain(|_, slot| {
            if slot.hold() != hold {
                return true;
            }
            if let Slot::State(slot) = slot {
                *bytes -= slot.size;
            }
            false
        });
    }

    pub(super) fn on_ack(&mut self, id: ConnId, n: u64) {
        let Some(conn) = self.conns.get_mut(&id) else { return };
        if conn.peer_acks != Some(true) || n < conn.acked || n > conn.sent {
            let reason = format!("ack {n} outside {}..{}", conn.acked, conn.sent);
            self.violation(id, &reason);
            return;
        }
        for _ in conn.acked..n {
            if let Some(size) = conn.unacked.pop_front() {
                conn.unacked_bytes -= size;
            }
        }
        conn.acked = n;
        self.flush(id);
    }

    /// Sends waiting slots in creation order while the window allows.
    pub(super) fn flush(&mut self, id: ConnId) {
        loop {
            let Some(conn) = self.conns.get_mut(&id) else { return };
            if conn.full() {
                return;
            }
            let Some((_, slot)) = conn.pending.shift_remove_index(0) else { return };
            let slot = match slot {
                Slot::Unit { hold } => {
                    self.send_unit(id, hold);
                    continue;
                }
                Slot::State(slot) => slot,
            };
            conn.pending_bytes -= slot.size;
            if slot.lazy {
                if slot.overflow == Overflow::Resend {
                    self.resend(id, slot.hold, &slot.m);
                }
                continue;
            }
            if let Some(base) = slot.base {
                conn.send(&Frame::State(base));
            }
            if !slot.ops.is_empty() {
                let ops = Some(slot.ops);
                let frame =
                    StateFrame { hold: slot.hold, m: slot.m, kind: StateKind::Ops, value: None, ops, error: None };
                conn.send(&Frame::State(frame));
            }
        }
    }
}

/// Drops what waits for a stream: a lazy reset replaces it (provider), or the relay asks downstream for one.
fn drop_slot(pending: &mut IndexMap<String, Slot>, bytes: &mut usize, key: &str) -> Option<(RelayId, String)> {
    let Some(Slot::State(slot)) = pending.get_mut(key) else { return None };
    *bytes -= slot.size;
    slot.base = None;
    slot.ops.clear();
    slot.size = 0;
    match slot.overflow {
        Overflow::Resend => {
            slot.lazy = true;
            None
        }
        Overflow::Resync(relay) => {
            let m = slot.m.clone();
            pending.shift_remove(key);
            Some((relay, m))
        }
    }
}
