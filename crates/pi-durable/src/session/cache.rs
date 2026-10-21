//! The loaded document cache, bounded by use: a document with no watch or document state attached leaves it once it was
//! not read or written for the retention period, and loads from Storage again on its next access. Eviction runs as a
//! job on the mutation line, so no transaction ever loses a tracker it holds.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::task::AbortHandle;

use super::core::{LoadedDocument, SessionCore};
use super::line::lock;
use crate::ids::DocumentId;

/// Default document retention: ten minutes, as for kept contexts.
pub const DEFAULT_DOCUMENT_RETENTION_MS: f64 = 600_000.0;

/// Longest delay one timer waits; a longer retention waits in several steps.
const MAX_TIMER_DELAY_MS: f64 = 2_147_483_647.0;

/// A retention as given, when it is a count of milliseconds, `0` or more (`f64::INFINITY` keeps documents); otherwise
/// the default.
pub fn document_retention(ms: Option<f64>) -> f64 {
    ms.filter(|ms| *ms >= 0.0).unwrap_or(DEFAULT_DOCUMENT_RETENTION_MS)
}

/// How many documents the cache holds, how many are idle, how many incarnations are pinned, and how many sweep jobs
/// were queued on the line so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DocumentCacheSizes {
    pub cached: usize,
    pub idle: usize,
    pub attached: usize,
    pub sweeps: usize,
}

/// The pending eviction: a timer for the oldest idle document, or a job on the line at once.
struct PendingSweep {
    at: f64,
    serial: u64,
    timer: Option<AbortHandle>,
}

#[derive(Default)]
pub(crate) struct DocCache {
    map: HashMap<String, Arc<LoadedDocument>>,
    /// Cached documents with no observer attached: by use order, oldest first, and by address with the time of use.
    idle_order: BTreeMap<u64, String>,
    idle: HashMap<String, (u64, f64)>,
    uses: u64,
    /// Observers attached per document incarnation; a cached incarnation with any is never evicted.
    attached: HashMap<DocumentId, usize>,
    sweep: Option<PendingSweep>,
    sweeps_started: u64,
    sweep_jobs: usize,
}

impl DocCache {
    pub fn get(&self, address_id: &str) -> Option<&Arc<LoadedDocument>> {
        self.map.get(address_id)
    }

    pub fn insert(&mut self, address_id: String, document: Arc<LoadedDocument>) {
        self.map.insert(address_id, document);
    }

    pub fn remove(&mut self, address_id: &str) {
        self.map.remove(address_id);
        self.forget_idle(address_id);
    }

    /// Drop every cached document; observers stay attached.
    pub fn clear(&mut self) {
        self.map.clear();
        self.idle.clear();
        self.idle_order.clear();
    }

    /// Drop everything, and the pending eviction.
    pub fn clear_all(&mut self) {
        self.clear();
        self.attached.clear();
        if let Some(timer) = self.sweep.take().and_then(|sweep| sweep.timer) {
            timer.abort();
        }
    }

    pub fn sizes(&self) -> DocumentCacheSizes {
        DocumentCacheSizes {
            cached: self.map.len(),
            idle: self.idle.len(),
            attached: self.attached.len(),
            sweeps: self.sweep_jobs,
        }
    }

    fn forget_idle(&mut self, address_id: &str) {
        if let Some((order, _)) = self.idle.remove(address_id) {
            self.idle_order.remove(&order);
        }
    }

    /// Note a use of a cached document: an unattached one becomes the newest idle one. Whether an eviction has to be
    /// scheduled: none is pending, or the one pending is later than the retention now allows.
    fn touch(&mut self, address_id: &str, now: f64, retention: f64) -> bool {
        let Some(document) = self.map.get(address_id) else { return false };
        if self.attached.contains_key(&document.record.id) {
            return false;
        }
        // A hot document moves to the back at most once per granule, a tenth of the retention up to 1 s: it may then
        // leave up to a granule early, which costs a reload, while a commit-heavy workload skips the reordering.
        if let Some((_, since)) = self.idle.get(address_id)
            && now - since < (retention / 10.0).min(1_000.0)
        {
            return false;
        }
        self.forget_idle(address_id);
        self.uses += 1;
        self.idle.insert(address_id.to_owned(), (self.uses, now));
        self.idle_order.insert(self.uses, address_id.to_owned());
        self.sweep.as_ref().is_none_or(|sweep| sweep.at > now + retention)
    }

    fn oldest_use(&self) -> Option<f64> {
        let (_, address_id) = self.idle_order.first_key_value()?;
        self.idle.get(address_id).map(|(_, since)| *since)
    }

    /// Evict the idle documents last used at or before `expired`, oldest first.
    fn evict_through(&mut self, expired: f64) {
        while let Some((_, address_id)) = self.idle_order.first_key_value() {
            let address_id = address_id.clone();
            let Some((_, since)) = self.idle.get(&address_id) else { break };
            if *since > expired {
                break;
            }
            self.remove(&address_id);
        }
    }
}

/// One observer's hold on a cached incarnation; released once, by detaching or by the incarnation retiring.
pub(crate) struct DocumentPin {
    core: Weak<SessionCore>,
    address_id: String,
    record_id: DocumentId,
    pinned: AtomicBool,
}

impl DocumentPin {
    pub fn release(&self) {
        if !self.pinned.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Some(core) = self.core.upgrade() {
            core.unpin_document(&self.address_id, self.record_id);
        }
    }
}

impl SessionCore {
    fn retention_ms(&self) -> f64 {
        document_retention((self.retention)())
    }

    /// Note a use of the cached document at `address_id`, and schedule an eviction when one is due.
    pub fn touch_document(&self, address_id: &str) {
        let (now, retention) = ((self.now)(), self.retention_ms());
        let schedule = lock(&self.documents).touch(address_id, now, retention);
        if schedule {
            self.schedule_document_sweep();
        }
    }

    /// Pin a cached incarnation for one observer; it is idle again once the last pin is released.
    pub fn pin_document(self: &Arc<Self>, address_id: &str, record_id: DocumentId) -> Arc<DocumentPin> {
        {
            let mut cache = lock(&self.documents);
            *cache.attached.entry(record_id).or_insert(0) += 1;
            cache.forget_idle(address_id);
        }
        Arc::new(DocumentPin {
            core: Arc::downgrade(self),
            address_id: address_id.to_owned(),
            record_id,
            pinned: AtomicBool::new(true),
        })
    }

    fn unpin_document(&self, address_id: &str, record_id: DocumentId) {
        let touch = {
            let mut cache = lock(&self.documents);
            let count = cache.attached.get(&record_id).copied().unwrap_or(1) - 1;
            if count > 0 {
                cache.attached.insert(record_id, count);
                false
            } else {
                cache.attached.remove(&record_id);
                cache.map.get(address_id).is_some_and(|document| document.record.id == record_id)
            }
        };
        if touch {
            self.touch_document(address_id);
        }
    }

    /// Schedule the eviction of the oldest idle document: on the line at once when its time has come, otherwise when
    /// the retention expires, by a timer that holds nothing alive.
    fn schedule_document_sweep(&self) {
        if lock(&self.closing).is_some() {
            return;
        }
        let (now, retention) = ((self.now)(), self.retention_ms());
        let (at, serial, previous) = {
            let mut cache = lock(&self.documents);
            let Some(since) = cache.oldest_use() else { return };
            let at = since + retention;
            if !at.is_finite() || cache.sweep.as_ref().is_some_and(|sweep| sweep.at <= at) {
                return;
            }
            cache.sweeps_started += 1;
            let serial = cache.sweeps_started;
            let previous = cache.sweep.replace(PendingSweep { at, serial, timer: None });
            (at, serial, previous)
        };
        if let Some(timer) = previous.and_then(|sweep| sweep.timer) {
            timer.abort();
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // Without a runtime only the end of each commit evicts.
            self.clear_pending_sweep(serial);
            return;
        };
        let delay = at - now;
        let weak = self.weak.clone();
        let task = runtime.spawn(async move {
            if delay > 0.0 {
                tokio::time::sleep(Duration::from_millis(delay.min(MAX_TIMER_DELAY_MS) as u64)).await;
            }
            if let Some(core) = weak.upgrade() {
                core.run_document_sweep(serial).await;
            }
        });
        if delay > 0.0 {
            let mut cache = lock(&self.documents);
            match cache.sweep.as_mut() {
                Some(sweep) if sweep.serial == serial => sweep.timer = Some(task.abort_handle()),
                _ => task.abort(),
            }
        }
    }

    fn clear_pending_sweep(&self, serial: u64) {
        let mut cache = lock(&self.documents);
        if cache.sweep.as_ref().is_some_and(|sweep| sweep.serial == serial) {
            cache.sweep = None;
        }
    }

    /// The pending eviction is due: run it as a job on the line, after the operation underway.
    async fn run_document_sweep(self: Arc<Self>, serial: u64) {
        lock(&self.documents).sweep_jobs += 1;
        let core = self.clone();
        let job = self.enqueue(async move {
            core.clear_pending_sweep(serial);
            if lock(&core.closing).is_none() {
                core.sweep_documents();
            }
            Ok(())
        });
        let _ = job.await;
    }

    /// On the line: evict the idle documents whose retention passed, oldest first, then schedule the next eviction.
    pub fn sweep_documents(&self) {
        let (now, retention) = ((self.now)(), self.retention_ms());
        lock(&self.documents).evict_through(now - retention);
        self.schedule_document_sweep();
    }

    /// Stop the pending eviction timer, when close begins.
    pub fn cancel_document_sweep(&self) {
        if let Some(timer) = lock(&self.documents).sweep.take().and_then(|sweep| sweep.timer) {
            timer.abort();
        }
    }
}
