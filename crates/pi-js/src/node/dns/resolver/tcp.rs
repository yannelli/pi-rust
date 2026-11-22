//! TCP streams shared by the questions in flight to one server from one source address. A
//! stream closes when its last question ends, so no idle socket stays open between queries.

mod link;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::{CancellationToken, DropGuard};

use super::super::wire::{Question, Reply};
use super::CONNREFUSED;
use super::transport::Attempt;

type Key = (IpAddr, SocketAddr);
type Outcome = Result<Reply, &'static str>;

/// Streams in use for one server, keyed by source and server address.
#[derive(Default)]
pub(super) struct TcpPool {
    shared: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    next_serial: u64,
    next_token: u64,
    links: HashMap<Key, Link>,
    /// Streams that ran out of free IDs, by serial; they close when their questions end.
    draining: HashMap<u64, Link>,
}

/// One stream and the questions waiting on it, by wire ID. Dropping it cancels its task,
/// which closes the socket.
struct Link {
    serial: u64,
    waiters: HashMap<u16, Waiter>,
    /// IDs of questions withdrawn before a reply; a late reply must not reach a newer question.
    retired: HashSet<u16>,
    outbox: mpsc::UnboundedSender<Queued>,
    _close: DropGuard,
}

/// A framed question; `token` ties it to the waiter that queued it.
struct Queued {
    wire: u16,
    token: u64,
    frame: Vec<u8>,
}

/// A question on a stream; `id` is the ID its packet was encoded with.
struct Waiter {
    token: u64,
    question: Question,
    id: u16,
    ttl: bool,
    done: oneshot::Sender<Outcome>,
}

fn lock(shared: &Mutex<State>) -> MutexGuard<'_, State> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Link {
    /// The first ID from `preferred` on that no waiting or retired question uses.
    fn free_id(&self, preferred: u16) -> Option<u16> {
        if self.waiters.len() + self.retired.len() == usize::from(u16::MAX) + 1 {
            return None;
        }
        (0..=u16::MAX)
            .map(|step| preferred.wrapping_add(step))
            .find(|id| !self.waiters.contains_key(id) && !self.retired.contains(id))
    }

    fn owns(&self, wire: u16, token: u64) -> bool {
        self.waiters.get(&wire).is_some_and(|waiter| waiter.token == token)
    }
}

impl State {
    fn link(&mut self, key: Key, serial: u64) -> Option<&mut Link> {
        if self.links.get(&key).is_some_and(|link| link.serial == serial) {
            return self.links.get_mut(&key);
        }
        self.draining.get_mut(&serial)
    }

    fn remove(&mut self, key: Key, serial: u64) -> Option<Link> {
        if self.links.get(&key).is_some_and(|link| link.serial == serial) {
            return self.links.remove(&key);
        }
        self.draining.remove(&serial)
    }

    /// Removes the link when no question waits on it; the caller drops it after unlocking.
    fn close_if_idle(&mut self, key: Key, serial: u64) -> Option<Link> {
        let idle = self.link(key, serial).is_some_and(|link| link.waiters.is_empty());
        if idle { self.remove(key, serial) } else { None }
    }

    /// The link for `key` with a free ID, starting one when none is open or the open one has
    /// no free ID left.
    fn usable(&mut self, shared: &Arc<Mutex<State>>, key: Key, preferred: u16) -> (&mut Link, u16) {
        if let Some(wire) = self.links.get(&key).and_then(|link| link.free_id(preferred)) {
            return (self.links.get_mut(&key).expect("link is open"), wire);
        }
        if let Some(full) = self.links.remove(&key) {
            self.draining.insert(full.serial, full);
        }
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1);
        let (outbox, queue) = mpsc::unbounded_channel();
        let close = CancellationToken::new();
        tokio::spawn(link::run(Arc::clone(shared), key, serial, queue, close.clone()));
        let link =
            Link { serial, waiters: HashMap::new(), retired: HashSet::new(), outbox, _close: close.drop_guard() };
        (self.links.entry(key).insert_entry(link).into_mut(), preferred)
    }
}

impl TcpPool {
    /// Queues the attempt's packet on the stream from its source to its server, and opens that
    /// stream when none is in use. The wire ID differs from `attempt.id` only on a clash.
    pub(super) fn send(&self, attempt: &Attempt<'_>) -> Result<Ticket, &'static str> {
        let packet = attempt.packet;
        let length = u16::try_from(packet.len()).ok().filter(|length| *length >= 2).ok_or("EBADQUERY")?;
        let key = (attempt.source, attempt.server);
        let mut state = lock(&self.shared);
        let token = state.next_token;
        state.next_token = state.next_token.wrapping_add(1);
        let (link, wire) = state.usable(&self.shared, key, attempt.id);
        let mut frame = Vec::with_capacity(packet.len() + 2);
        frame.extend_from_slice(&length.to_be_bytes());
        frame.extend_from_slice(&wire.to_be_bytes());
        frame.extend_from_slice(&packet[2..]);
        link.outbox.send(Queued { wire, token, frame }).map_err(|_| CONNREFUSED)?;
        let (done, outcome) = oneshot::channel();
        let waiter = Waiter { token, question: attempt.question.clone(), id: attempt.id, ttl: attempt.ttl, done };
        link.waiters.insert(wire, waiter);
        Ok(Ticket { shared: Arc::clone(&self.shared), key, serial: link.serial, wire, token, outcome })
    }
}

/// One question on a stream. Dropping it before its reply withdraws the question, retires its
/// wire ID, and closes the stream when no other question waits on it.
pub(super) struct Ticket {
    shared: Arc<Mutex<State>>,
    key: Key,
    serial: u64,
    wire: u16,
    token: u64,
    outcome: oneshot::Receiver<Outcome>,
}

impl Ticket {
    /// The reply to this question, the code of a malformed frame on the stream, or
    /// `ECONNREFUSED` when the stream failed or closed first.
    pub(super) async fn reply(&mut self) -> Outcome {
        (&mut self.outcome).await.unwrap_or(Err(CONNREFUSED))
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let closed = {
            let mut state = lock(&self.shared);
            if let Some(link) = state.link(self.key, self.serial)
                && link.owns(self.wire, self.token)
            {
                link.waiters.remove(&self.wire);
                link.retired.insert(self.wire);
            }
            state.close_if_idle(self.key, self.serial)
        };
        drop(closed);
    }
}
