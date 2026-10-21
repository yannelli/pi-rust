//! The link side of reloads: the participants of a cluster reload, `prepare` frames and the votes that answer
//! them, `commit`, `abort` and `retire` frames, hellos a reload deferred, and reconciling a child host that
//! connected behind this host's code version.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::oneshot;

use super::conn::{Conn, unavailable};
use super::net::{ConnId, Msg, Net, NetHandle, Work};
use crate::error::{ChordError, ChordResult};
use crate::host2::errors::ChordErrorCode;
use crate::host2::host::HostImpl;
use crate::host2::object::HostCore;
use crate::host2::wire::{Frame, Prepare, Vote, WireReloadOptions, WireReloadReport};

/// A child's vote: what it prepared, or why it could not.
pub(crate) type VoteResult = ChordResult<Option<WireReloadReport>>;

/// How long a reconciliation waits for the child's vote.
const RECONCILE_VOTE_DEADLINE: Duration = Duration::from_secs(30);

static RELOAD_SEQ: AtomicU64 = AtomicU64::new(0);

/// Reload ids on links, process-wide (upstream's module counter).
pub(crate) fn next_reload_id() -> u64 {
    RELOAD_SEQ.fetch_add(1, Ordering::Relaxed) + 1
}

/// A child host taking part in a cluster reload.
#[derive(Clone, Debug)]
pub(crate) struct Participant {
    pub conn: ConnId,
    pub name: String,
}

/// What one step of a reconciliation found on the link.
enum Step {
    Done,
    Vote(oneshot::Receiver<VoteResult>),
}

impl NetHandle {
    pub(super) fn run(&self, work: impl FnOnce(&mut Net) + Send + 'static) {
        self.post(Msg::Run(Work(Box::new(work))));
    }

    async fn ask<T: Send + 'static>(&self, work: impl FnOnce(&mut Net) -> T + Send + 'static) -> Option<T> {
        let (reply, wait) = oneshot::channel();
        self.run(move |net| {
            let _ = reply.send(work(net));
        });
        wait.await.ok()
    }

    /// Whether any child host is connected, and the ones in step now (open and synced).
    pub(crate) async fn child_hosts(&self) -> (bool, Vec<Participant>) {
        let found = self.ask(|net| {
            let children: Vec<&Conn> = net.conns.values().filter(|conn| conn.child_host()).collect();
            let synced = children.iter().filter(|conn| conn.open() && conn.synced);
            let name = |conn: &Conn| conn.name().unwrap_or("child").to_string();
            let synced = synced.map(|conn| Participant { conn: conn.id, name: name(conn) }).collect();
            (!children.is_empty(), synced)
        });
        found.await.unwrap_or_default()
    }

    /// The participants whose link is still open.
    pub(crate) async fn still_open(&self, participants: Vec<Participant>) -> Vec<Participant> {
        let open = move |net: &mut Net| {
            let open = |participant: &Participant| net.conns.get(&participant.conn).is_some_and(Conn::open);
            participants.into_iter().filter(open).collect()
        };
        self.ask(open).await.unwrap_or_default()
    }

    /// Sends `prepare` to each participant; each receiver gets that child's vote (or the close of its link).
    pub(crate) fn prepare(&self, frames: Vec<(ConnId, Prepare)>) -> Vec<oneshot::Receiver<VoteResult>> {
        let (waiters, receivers): (Vec<_>, Vec<_>) = frames.iter().map(|_| oneshot::channel()).unzip();
        self.run(move |net| {
            for ((conn, prepare), waiter) in frames.into_iter().zip(waiters) {
                net.expect_vote(conn, prepare, waiter);
            }
        });
        receivers
    }

    /// Stops waiting for the votes of `reload` and tells these children to roll back.
    pub(crate) fn abort(&self, conns: Vec<ConnId>, reload: u64) {
        self.run(move |net| {
            for conn in conns {
                if let Some(found) = net.conns.get_mut(&conn) {
                    found.votes.remove(&reload);
                }
                net.send(conn, &Frame::Abort { reload });
            }
        });
    }

    /// Tells these children to commit `reload`: they run `code_version` from now on.
    pub(crate) fn commit(&self, conns: Vec<ConnId>, reload: u64, code_version: u64) {
        self.run(move |net| {
            for conn in conns {
                net.send(conn, &Frame::Commit { reload });
                if let Some(found) = net.conns.get_mut(&conn) {
                    found.peer_code_version = Some(code_version);
                }
            }
        });
    }

    /// This host retired `code_version`: its child hosts may retire it too.
    pub(crate) fn retired(&self, code_version: u64) {
        self.run(move |net| {
            let children: Vec<ConnId> = net.conns.values().filter(|conn| conn.child_host()).map(|c| c.id).collect();
            for conn in children {
                net.send(conn, &Frame::Retire { code_version });
            }
        });
    }

    /// The host's reload ended: the hellos it deferred are decided now.
    pub(crate) fn reload_ended(&self) {
        self.run(|net| {
            let deferred = std::mem::take(&mut net.deferred_hellos);
            for (conn, code_version) in deferred {
                if net.conns.get(&conn).is_some_and(Conn::open) {
                    net.peer_hello(conn, code_version);
                }
            }
        });
    }

    /// Answers the parent's `prepare` over `conn`.
    pub(crate) fn vote(&self, conn: ConnId, vote: Vote) {
        self.post(Msg::Send { conn, frame: Frame::Vote(vote) });
    }
}

impl Net {
    fn expect_vote(&mut self, id: ConnId, prepare: Prepare, waiter: oneshot::Sender<VoteResult>) {
        let Some(conn) = self.conns.get_mut(&id).filter(|conn| conn.open()) else {
            let _ = waiter.send(Err(unavailable("link to child closed")));
            return;
        };
        conn.votes.insert(prepare.reload, waiter);
        conn.send(&Frame::Prepare(prepare));
    }

    /// Tells a child host that is now in step which code versions this host retired before. Without it, a child
    /// reconciled after those retirements would wait for the retire deadline at its next reload.
    pub(super) fn announce_retired(&mut self, id: ConnId) {
        let Some(host) = self.host() else { return };
        let retired = host.retired_floor();
        if retired > 0 {
            self.send(id, &Frame::Retire { code_version: retired as u64 });
        }
    }

    /// Starts reconciling a child host behind this host's code version; a failure closes its link.
    pub(super) fn reconcile(&mut self, id: ConnId) {
        let Some(host) = self.host() else { return };
        let (weak, net) = (Arc::downgrade(&host), host.net().clone());
        self.handle.spawn(async move {
            if let Err(error) = reconcile_child(weak.clone(), net.clone(), id).await {
                if let Some(host) = weak.upgrade() {
                    host.report(error);
                }
                net.run(move |net| net.close_link(id, "reconciliation failed"));
            }
        });
    }
}

/// Brings a child that connected behind this host's code version up to it: a reload of the child's modules (the
/// ones cluster reloads gave its name) to this host's current code version, prepared, voted and committed like a
/// cluster reload. Until then the child is fenced. Its old code is never relabeled as a newer code version.
async fn reconcile_child(host: Weak<HostImpl>, net: NetHandle, id: ConnId) -> ChordResult<()> {
    loop {
        let Some(found) = host.upgrade() else { return Ok(()) };
        // Not during a reload here: the child is not one of its participants. Recheck once it ended.
        if found.reloading() {
            let ended = found.when_reload_ended();
            drop(found);
            ended.await;
            continue;
        }
        let code_version = found.current_code_version();
        let name = net.ask(move |net| net.conns.get(&id).map(Conn::shown_name)).await.flatten();
        let Some(name) = name else { return Ok(()) };
        let (plugins, remove) = found.remote_modules_for(&name);
        drop(found);
        let reload = next_reload_id();
        let options = WireReloadOptions { remove: Some(remove), gate_deadline: None, retire_deadline: None };
        let prepare = Prepare { reload, code_version: code_version as u64, plugins, options };
        let step = net.ask(move |net| {
            let Some(conn) = net.conns.get(&id).filter(|conn| conn.open()) else { return Step::Done };
            if conn.peer_code_version == Some(code_version as u64) {
                net.mark_synced(id, code_version as u64);
                net.announce_retired(id);
                return Step::Done;
            }
            // Before `prepare`: the child's prepare first retires its older code version, which waits for this.
            net.announce_retired(id);
            let (waiter, vote) = oneshot::channel();
            net.expect_vote(id, prepare, waiter);
            Step::Vote(vote)
        });
        let Some(Step::Vote(vote)) = step.await else { return Ok(()) };
        match tokio::time::timeout(RECONCILE_VOTE_DEADLINE, vote).await {
            Ok(Ok(vote)) => {
                vote?;
            }
            Ok(Err(_)) => return Err(unavailable(format!("link to {name} closed"))),
            Err(_) => {
                net.run(move |net| {
                    if let Some(conn) = net.conns.get_mut(&id) {
                        conn.votes.remove(&reload);
                    }
                });
                let message = format!("votes missing after {} ms", RECONCILE_VOTE_DEADLINE.as_millis());
                return Err(ChordError::with_reason(ChordErrorCode::Aborted, "vote_deadline", message));
            }
        }
        let Some(found) = host.upgrade() else { return Ok(()) };
        if found.current_code_version() != code_version {
            // This host moved on meanwhile: abort, and try again with the newer code version.
            net.abort(vec![id], reload);
            continue;
        }
        net.commit(vec![id], reload, code_version as u64);
    }
}
