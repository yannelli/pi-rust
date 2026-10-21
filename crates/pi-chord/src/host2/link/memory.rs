//! Two connected in-memory links (tests). Each frame takes a JSON round trip, packed like an IPC message, and one
//! task per direction delivers frames in order and asynchronously, like a real transport.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use tokio::sync::mpsc;

use super::pack::{pack_object, parse_frame};
use super::{BaseLink, Transport};
use crate::host2::wire::Frame;
use crate::json::utf16_len;

enum Pumped {
    Data(String),
    End,
}

/// The sending half of one memory link.
pub struct MemoryTransport {
    tx: mpsc::UnboundedSender<Pumped>,
    severed: Arc<AtomicBool>,
}

impl Transport for MemoryTransport {
    fn transmit(&self, frame: &Frame) -> Option<usize> {
        if self.severed.load(Ordering::SeqCst) {
            return None;
        }
        let data = pack_object(frame, true).to_string();
        let size = utf16_len(&data);
        let _ = self.tx.send(Pumped::Data(data));
        Some(size)
    }

    fn shutdown(&self) {
        let _ = self.tx.send(Pumped::End);
    }
}

pub type MemoryLink = BaseLink<MemoryTransport>;

impl BaseLink<MemoryTransport> {
    /// Simulates a dead network: frames are dropped silently in both directions and no close is signalled.
    pub fn sever(&self) {
        self.transport().severed.store(true, Ordering::SeqCst);
    }
}

async fn pump(mut rx: mpsc::UnboundedReceiver<Pumped>, peer: Weak<MemoryLink>, severed: Arc<AtomicBool>) {
    while let Some(item) = rx.recv().await {
        let Some(peer) = peer.upgrade() else { return };
        match item {
            Pumped::Data(data) if !severed.load(Ordering::SeqCst) => {
                if let Ok(received) = parse_frame(&data) {
                    peer.deliver(received);
                }
            }
            Pumped::Data(_) => {}
            Pumped::End => peer.ended("peer closed"),
        }
    }
}

/// Two connected in-memory links. Must be called within a Tokio runtime.
pub fn memory_link_pair() -> (Arc<MemoryLink>, Arc<MemoryLink>) {
    let severed = Arc::new(AtomicBool::new(false));
    let (a_tx, a_rx) = mpsc::unbounded_channel();
    let (b_tx, b_rx) = mpsc::unbounded_channel();
    let a = Arc::new(BaseLink::new(MemoryTransport { tx: a_tx, severed: severed.clone() }));
    let b = Arc::new(BaseLink::new(MemoryTransport { tx: b_tx, severed: severed.clone() }));
    tokio::spawn(pump(a_rx, Arc::downgrade(&b), severed.clone()));
    tokio::spawn(pump(b_rx, Arc::downgrade(&a), severed));
    (a, b)
}
