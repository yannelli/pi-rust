//! Deferred commits (spec section 4.1): a commit held in memory, its changes folded per document and pinned in the
//! cache until a commit that reads or writes the document, its deadline, a drop of its tracker, or close stores them.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pi_chord::delta::{Op, PendingOps, PendingTake};
use pi_chord::{BACKGROUND_CONTEXT, Context, Json, without_abort_signal};

use super::ObservedCommit;
use super::cache::DocumentPin;
use super::core::{LoadedDocument, SessionCore};
use super::guarded::FailureLatch;
use super::line::lock;
use super::plans::PlanRecord;
use super::tx::Tx;
use crate::errors::{DurableError, DurableResult};
use crate::ids::DocumentId;
use crate::timers::Disarm;
use crate::types::{
    CommitChange, CommitPersistence, CommitPublication, DocumentCommitChange, DocumentContent, DocumentFork,
    DocumentHistory, DocumentScope, Storage, StorageWrite,
};

/// Default `within_ms` of a deferred commit, `settings.progress.persist_within_ms`.
pub const DEFAULT_PERSIST_WITHIN_MS: f64 = 1_000.0;

/// Longest delay one timer waits.
const MAX_TIMER_DELAY_MS: f64 = 2_147_483_647.0;

/// A deferred commit's default window as given, when it is a count of milliseconds, `0` or more (infinity waits for
/// a reading or writing commit or close); otherwise the default.
pub fn persist_within(ms: Option<f64>) -> f64 {
    ms.filter(|ms| *ms >= 0.0).unwrap_or(DEFAULT_PERSIST_WITHIN_MS)
}

/// How long a commit may stay in memory before it is stored; `None` when it is immediate.
pub(crate) fn deferral(core: &SessionCore, persistence: CommitPersistence) -> DurableResult<Option<f64>> {
    match persistence {
        CommitPersistence::Immediate => Ok(None),
        CommitPersistence::Deferred { within_ms: None } => Ok(Some(persist_within((core.persist_within)()))),
        CommitPersistence::Deferred { within_ms: Some(ms) } if ms >= 0.0 => Ok(Some(ms)),
        CommitPersistence::Deferred { .. } => {
            Err(DurableError::type_error("CommitPersistence.withinMs must be a number of milliseconds, 0 or more"))
        }
    }
}

/// The changes of a cached incarnation that deferred commits adopted in memory and the Session has not stored yet.
pub(crate) struct Hold {
    loaded: Arc<LoadedDocument>,
    pending: PendingOps,
    /// Set when a held commit selected a base (`checkpoint_when`): their store writes the whole value.
    base: bool,
    /// When the earliest of its held commits must be stored, on the Session clock.
    deadline: f64,
    pin: Arc<DocumentPin>,
}

/// The armed store of held documents: a foreground timer for the earliest deadline, `fired` once its job is queued.
struct HoldTimer {
    at: f64,
    disarm: Option<Disarm>,
    fired: Arc<AtomicBool>,
}

impl HoldTimer {
    fn disarm(mut self) {
        if let Some(disarm) = self.disarm.take() {
            disarm();
        }
    }
}

/// Documents with held changes, by incarnation, and the timer of their earliest deadline.
#[derive(Default)]
pub(crate) struct Holds {
    map: BTreeMap<DocumentId, Hold>,
    timer: Option<HoldTimer>,
}

impl Holds {
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn contains(&self, id: DocumentId) -> bool {
        self.map.contains_key(&id)
    }
}

/// Take a hold's changes after `ops`, a commit's own on top of them, as one stored content for an incarnation now at
/// `value`, and the operations a reader of the stored state applies to reach it: the folded ones, or a root replacement
/// after the queue overflowed, which stores a base, as does a hold whose commits selected one.
fn store_held(hold: &mut Hold, ops: Vec<Op>, value: Json, version: u32) -> (DocumentContent, Vec<Op>) {
    hold.pending.push(ops);
    match hold.pending.take() {
        PendingTake::Replace => (DocumentContent::Base { version, value: value.clone() }, vec![Op::Replace(value)]),
        PendingTake::Ops(ops) if hold.base => (DocumentContent::Base { version, value }, ops),
        PendingTake::Ops(ops) => (DocumentContent::Delta { version, ops: ops.clone() }, ops),
    }
}

/// What one held commit changed in one incarnation: the incarnation, its operations, and whether they selected a base.
pub(crate) type HeldChange = (Arc<LoadedDocument>, Vec<Op>, bool);

impl Tx {
    /// What a deferred commit holds in memory instead of storing, each changed incarnation with its operations and
    /// whether they selected a base; `None` when the commit must be stored. It may be held only when every write
    /// changes an existing conversation document (history `Latest`, fork `Initial`; concise-ignore) as stored.
    pub(crate) fn holdable(&self, writes: &[StorageWrite]) -> Option<Vec<HeldChange>> {
        if writes.iter().any(|write| !matches!(write, StorageWrite::DocumentChange { .. })) {
            return None;
        }
        let state = lock(&self.0.state);
        let mut held = Vec::new();
        for plan in &state.plans {
            if plan.content.is_none() && !plan.retire {
                continue;
            }
            let Some(StorageWrite::DocumentChange { content, .. }) = &plan.content else { return None };
            let change = plan.change.as_ref().filter(|_| !plan.retire)?;
            let loaded = change.loaded.as_ref()?;
            let record = &loaded.record;
            let conversation = matches!(record.scope, DocumentScope::Conversation { .. });
            let latest = record.history == Some(DocumentHistory::Latest);
            if !conversation || !latest || record.fork != Some(DocumentFork::Initial) {
                return None;
            }
            // A migration stores a base at once.
            if loaded.stored_version() != change.version {
                return None;
            }
            let base = matches!(content, DocumentContent::Base { .. });
            held.push((loaded.clone(), change.prepared.ops.clone(), base));
        }
        Some(held)
    }

    /// Adopt a held commit in memory only. Trackers advance by pointer swap; stored versions and delta counts stay, as
    /// nothing was stored. Returns its publication for the Session's own observers.
    pub(crate) fn adopt_held(&self) -> DurableResult<Vec<DocumentCommitChange>> {
        let plans = std::mem::take(&mut lock(&self.0.state).plans);
        let mut publications = Vec::new();
        for plan in plans {
            // Every plan of a held commit changes or reads a loaded incarnation (`holdable()`).
            let (Some(change), PlanRecord::Committed(record)) = (plan.change, plan.record) else { continue };
            if change.prepared.ops.is_empty() {
                change.prepared.abort();
                continue;
            }
            lock(&change.tracker).adopt(&change.prepared)?;
            let (version, value, ops) = (Some(change.version), Some(change.prepared.value), change.prepared.ops);
            let conversation_id = plan.conversation_id;
            publications.push(DocumentCommitChange::Document { record, conversation_id, version, value, ops });
        }
        Ok(publications)
    }

    /// Store the held changes of every incarnation this commit reads or writes in front of its own, so a stored
    /// document never misses an operation its later ones build on. Called right before Storage admission, after every
    /// fallible step, because it empties the holds. Returns the incarnations whose holds it took.
    pub(crate) fn take_held(&self, writes: &mut Vec<StorageWrite>) -> Vec<DocumentId> {
        let mut holds = lock(&self.0.core.holds);
        if holds.map.is_empty() {
            return Vec::new();
        }
        let mut state = lock(&self.0.state);
        let mut taken = Vec::new();
        for plan in &mut state.plans {
            // A new incarnation holds nothing yet.
            let PlanRecord::Committed(record) = &plan.record else { continue };
            let (id, scope) = (record.id, record.scope);
            let Some(hold) = holds.map.get_mut(&id) else { continue };
            taken.push(id);
            let own = match &plan.content {
                Some(StorageWrite::DocumentChange { content, .. }) => Some(content.clone()),
                _ => None,
            };
            let (ops, value) = match &plan.change {
                Some(change) => (change.prepared.ops.clone(), change.prepared.value.clone()),
                None => (Vec::new(), hold.loaded.value()),
            };
            let version = hold.loaded.value_version;
            let (content, stored_ops) = store_held(hold, ops, value, version);
            // A base this commit selected or must write covers what was held.
            let has_own = own.is_some();
            let content = match own {
                Some(base @ DocumentContent::Base { .. }) => base,
                _ => content,
            };
            let write = StorageWrite::DocumentChange { id, content };
            let same = |other: &StorageWrite| matches!(other, StorageWrite::DocumentChange { id: at, .. } if *at == id);
            let retiring = |other: &StorageWrite| matches!(other, StorageWrite::DocumentRetire { id: at } if *at == id);
            match (writes.iter().position(same), writes.iter().position(retiring)) {
                (Some(at), _) => writes[at] = write.clone(),
                (None, Some(at)) => writes.insert(at, write.clone()),
                (None, None) => writes.push(write.clone()),
            }
            if !plan.retire {
                plan.held = Some((stored_ops, has_own));
            }
            plan.content = Some(write);
            // Held documents are conversation documents; one this commit only read has no owner resolved yet.
            if let DocumentScope::Conversation { conversation_id } = scope {
                plan.conversation_id = Some(conversation_id);
            }
        }
        taken
    }
}

impl SessionCore {
    /// Hold a deferred commit: adopt it in memory, queue its changes per document, pinned, by the earliest deadline of
    /// the document's held commits, and show it to the Session's own observers.
    pub(crate) fn hold(self: &Arc<Self>, tx: &Tx, held: Vec<HeldChange>, within: f64, context: &Context) {
        let documents = match tx.adopt_held() {
            Ok(documents) => documents,
            Err(error) => return self.fail_in(error, Some(context)),
        };
        let deadline = (self.now)() + within;
        {
            let mut holds = lock(&self.holds);
            for (loaded, ops, base) in held {
                let id = loaded.record.id;
                let hold = holds.map.entry(id).or_insert_with(|| Hold {
                    pin: self.pin_document(&loaded.address_id, id),
                    loaded: loaded.clone(),
                    pending: PendingOps::new(),
                    base: false,
                    deadline,
                });
                hold.pending.push(ops);
                hold.base |= base;
                hold.deadline = hold.deadline.min(deadline);
            }
        }
        self.arm_hold_timer();
        let observed = ObservedCommit::Memory(documents.into_iter().map(CommitChange::Document).collect());
        for listener in self.internal_listeners() {
            if let Err(error) = listener(&observed, context) {
                self.fail_in(error, Some(context));
            }
        }
    }

    /// On the line: store every held document in one storage transaction.
    pub(crate) async fn store_all_holds(self: &Arc<Self>, context: &Context) -> DurableResult<()> {
        let ids: Vec<DocumentId> = lock(&self.holds).map.keys().copied().collect();
        self.store_holds(&ids, context).await
    }

    /// On the line: store the held changes of `ids` in one storage transaction. Memory already has them, so only
    /// `subscribe_commits()` listeners see it. A failure fails the Session, as every Storage failure does.
    pub(crate) async fn store_holds(self: &Arc<Self>, ids: &[DocumentId], context: &Context) -> DurableResult<()> {
        let (mut writes, mut changes, mut stored) = (Vec::new(), Vec::new(), Vec::new());
        {
            let mut holds = lock(&self.holds);
            for id in ids {
                let Some(hold) = holds.map.get_mut(id) else { continue };
                let (loaded, value) = (hold.loaded.clone(), hold.loaded.value());
                let version = loaded.value_version;
                let (content, ops) = store_held(hold, Vec::new(), value.clone(), version);
                stored.push((loaded.clone(), matches!(content, DocumentContent::Base { .. })));
                writes.push(StorageWrite::DocumentChange { id: *id, content });
                let record = loaded.record.clone();
                let conversation_id = match record.scope {
                    DocumentScope::Conversation { conversation_id } => Some(conversation_id),
                    _ => None,
                };
                let (version, value) = (Some(version), Some(value));
                let change = DocumentCommitChange::Document { record, conversation_id, version, value, ops };
                changes.push(CommitChange::Document(change));
            }
        }
        if writes.is_empty() {
            return Ok(());
        }
        let adopted = AtomicBool::new(false);
        let adopt = |_| {
            adopted.store(true, Ordering::SeqCst);
            for (loaded, base) in &stored {
                let mut meta = lock(&loaded.meta);
                meta.deltas_since_base = if *base { 0 } else { meta.deltas_since_base + 1 };
            }
        };
        let seq = self.storage.commit(&writes, &adopt, &without_abort_signal(context)).await?;
        if !adopted.load(Ordering::SeqCst) {
            let error = DurableError::error("Storage committed without adopting the batch");
            self.fail_in(error.clone(), Some(context));
            return Err(error);
        }
        let ids: Vec<DocumentId> = stored.iter().map(|(loaded, _)| loaded.record.id).collect();
        self.release_holds(&ids);
        self.publish_stored(&CommitPublication { seq, changes }, &self.host_listeners(), context);
        Ok(())
    }

    /// After their store: forget the holds of `ids`, unpin their documents, and move the timer to the next deadline.
    pub(crate) fn release_holds(self: &Arc<Self>, ids: &[DocumentId]) {
        let released: Vec<Hold> = {
            let mut holds = lock(&self.holds);
            ids.iter().filter_map(|id| holds.map.remove(id)).collect()
        };
        for hold in released {
            hold.pin.release();
        }
        self.arm_hold_timer();
    }

    /// At close: forget every hold and its timer.
    pub(crate) fn clear_holds(&self) {
        let (map, timer) = {
            let mut holds = lock(&self.holds);
            (std::mem::take(&mut holds.map), holds.timer.take())
        };
        drop(map);
        if let Some(timer) = timer {
            timer.disarm();
        }
    }

    /// When close begins: stop the timer; close stores what is held itself.
    pub(crate) fn disarm_hold_timer(&self) {
        let timer = lock(&self.holds).timer.take();
        if let Some(timer) = timer {
            timer.disarm();
        }
    }

    /// Keep a foreground timer for the earliest deadline of the held documents. When it fires, every held document is
    /// stored in one storage transaction: a deadline is the latest time, and the others cost little more in the same
    /// transaction. Once fired, it stays until its job ran.
    pub(crate) fn arm_hold_timer(self: &Arc<Self>) {
        if lock(&self.closing).is_some() {
            return;
        }
        let stale = {
            let mut holds = lock(&self.holds);
            let at = holds.map.values().map(|hold| hold.deadline).fold(f64::INFINITY, f64::min);
            if holds.timer.as_ref().is_some_and(|timer| timer.fired.load(Ordering::SeqCst) || timer.at == at) {
                return;
            }
            let stale = holds.timer.take();
            if at.is_finite() {
                let fired = Arc::new(AtomicBool::new(false));
                let timer = HoldTimer { at, disarm: None, fired: fired.clone() };
                holds.timer = Some(timer);
                drop(holds);
                let delay = (at - (self.now)()).clamp(0.0, MAX_TIMER_DELAY_MS);
                let disarm = self.timers.foreground(self.hold_timer_callback(fired.clone()), delay);
                let mut holds = lock(&self.holds);
                match holds.timer.as_mut() {
                    Some(timer) if Arc::ptr_eq(&timer.fired, &fired) => timer.disarm = Some(disarm),
                    _ => disarm(),
                }
            }
            stale
        };
        if let Some(stale) = stale {
            stale.disarm();
        }
    }

    /// What the hold timer runs: queue a job that stores every held document, unless close or a failure came first.
    fn hold_timer_callback(&self, fired: Arc<AtomicBool>) -> Box<dyn FnOnce() + Send> {
        let weak = self.weak.clone();
        Box::new(move || {
            let Some(core) = weak.upgrade() else { return };
            fired.store(true, Ordering::SeqCst);
            let job_core = core.clone();
            drop(core.enqueue(async move {
                {
                    let mut holds = lock(&job_core.holds);
                    if holds.timer.as_ref().is_some_and(|timer| Arc::ptr_eq(&timer.fired, &fired)) {
                        holds.timer = None;
                    }
                }
                // Close stores what is held itself.
                if lock(&job_core.closing).is_some() || job_core.failure().is_some() {
                    return Ok(());
                }
                job_core.store_all_holds(&BACKGROUND_CONTEXT).await
            }));
        })
    }
}
