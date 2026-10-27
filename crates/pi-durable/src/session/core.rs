//! Session kernel: the mutation line, the loaded document tracker cache, commit, close, and failure.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use pi_chord::delta::Tracker;
use pi_chord::{BACKGROUND_CONTEXT, Context, Json, Unsubscribe, without_abort_signal};
use pi_telemetry::SpanAttributes;
use tokio::sync::watch;

use super::cache::{DocCache, DocumentCacheSizes};
use super::guarded::{FailureLatch, GuardedStorage};
use super::holds::{Holds, deferral};
use super::line::{Line, SharedResult, await_with_context, lock, spawn_shared};
use super::tx::{TransactionScope, Tx};
use super::{CommitListener, ObservedCommit, ObservedCommitListener, SessionEnd, SessionHooks};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{EntryId, Seq};
use crate::timers::{SetTimer, Timers};
use crate::types::{
    CommitChange, CommitOptions, CommitPublication, DocumentCommitChange, DocumentRecord, HostCallback, ReportInfo,
    Storage, StorageWrite,
};

/// One committed document incarnation owned by the tracker cache.
pub(crate) struct LoadedDocument {
    pub address_id: String,
    pub record: DocumentRecord,
    /// Definition version whose shape the tracked value has; access with another version reloads from Storage.
    pub value_version: u32,
    pub tracker: Arc<Mutex<Tracker>>,
    pub meta: Mutex<LoadedMeta>,
}

pub(crate) struct LoadedMeta {
    /// Persisted definition version; older while the tracked value is migrated only in memory.
    pub stored_version: u32,
    /// Stored deltas after the newest base; advanced by adoption so the predicate needs no read.
    pub deltas_since_base: u32,
}

impl LoadedDocument {
    pub fn value(&self) -> Json {
        lock(&self.tracker).value().clone()
    }

    pub fn stored_version(&self) -> u32 {
        lock(&self.meta).stored_version
    }
}

/// A conversation document's current incarnation and value, read on the line.
#[derive(Clone, Debug)]
pub struct OnLineDocument {
    pub record: DocumentRecord,
    pub version: u32,
    pub value: Json,
}

type CloseListener = Arc<dyn Fn() + Send + Sync>;

/// Settings a Session reads at each use, and whether it refuses writes.
pub(crate) struct Settings {
    /// The document retention in milliseconds; `None` means the default.
    pub retention: Arc<dyn Fn() -> Option<f64> + Send + Sync>,
    /// The default window of a deferred commit in milliseconds; `None` means the default.
    pub persist_within: Arc<dyn Fn() -> Option<f64> + Send + Sync>,
    pub read_only: bool,
    /// Arms the foreground timer of held documents; default a Tokio timer.
    pub set_timer: Option<SetTimer>,
}

/// What adoption publishes: to the Session's own observers, and the stored changes to `subscribe_commits()`.
pub(crate) struct AdoptedDocuments {
    /// This commit's own changes, as memory adopted them.
    pub observed: Vec<DocumentCommitChange>,
    /// What Storage got, when the commit also stored held changes, which precede its own; `None`: `observed`.
    pub stored: Option<Vec<DocumentCommitChange>>,
}

pub(crate) struct SessionCore {
    pub storage: Arc<GuardedStorage>,
    pub documents: Mutex<DocCache>,
    commit_listeners: Mutex<Vec<(u64, CommitListener)>>,
    /// The Session's own listeners: they keep memory in step with storage, so a failure fails it.
    internal_listeners: Mutex<Vec<(u64, ObservedCommitListener)>>,
    close_listeners: Mutex<Vec<(u64, CloseListener)>>,
    next_listener: AtomicU64,
    line: Line,
    pub(super) closing: Mutex<Option<SharedResult<()>>>,
    failure: Mutex<Option<DurableError>>,
    failed: watch::Sender<Option<DurableError>>,
    closed: watch::Sender<Option<SessionEnd>>,
    pub hooks: Arc<dyn SessionHooks>,
    pub now: Arc<dyn Fn() -> f64 + Send + Sync>,
    /// The document retention in milliseconds, read at each use; `None` means the default.
    pub(super) retention: Arc<dyn Fn() -> Option<f64> + Send + Sync>,
    /// The default window of a deferred commit, read at each use; `None` means the default.
    pub(super) persist_within: Arc<dyn Fn() -> Option<f64> + Send + Sync>,
    /// Documents with held changes of deferred commits, each pinned in the cache until they are stored.
    pub(super) holds: Mutex<Holds>,
    pub(super) timers: Timers,
    /// Refuse every commit with writes (`HarnessOptions::read_only`).
    read_only: bool,
    pub(super) weak: Weak<SessionCore>,
}

/// Session kernel handle; cheap to clone. Only committed state is observable.
#[derive(Clone)]
pub struct Session(pub(crate) Arc<SessionCore>);

impl FailureLatch for SessionCore {
    fn failure(&self) -> Option<DurableError> {
        lock(&self.failure).clone()
    }

    fn fail(&self, error: DurableError) {
        self.fail_in(error, None);
    }
}

impl SessionCore {
    /// Fail the Session; the report's telemetry event goes to `context`'s span.
    pub fn fail_in(&self, error: DurableError, context: Option<&Context>) {
        {
            let mut failure = lock(&self.failure);
            if failure.is_some() {
                return;
            }
            *failure = Some(error.clone());
        }
        self.failed.send_replace(Some(error.clone()));
        if let Some(core) = self.weak.upgrade() {
            // Seal first, so a report handler that calls back finds the Session failed.
            drop(Session(core).close(&BACKGROUND_CONTEXT));
        }
        self.hooks.report(&error, &ReportInfo::SessionFailed, context);
    }

    /// A listener's error, which never fails what ran it.
    pub fn report(&self, error: &DurableError) {
        self.hooks.report(error, &ReportInfo::ListenerError, None);
    }

    pub fn assert_healthy(&self) -> DurableResult<()> {
        match self.failure() {
            Some(cause) => Err(DurableError::session_failed(cause)),
            None => Ok(()),
        }
    }

    pub fn assert_usable(&self) -> DurableResult<()> {
        self.assert_healthy()?;
        if lock(&self.closing).is_some() {
            return Err(DurableError::error("Session is closed"));
        }
        Ok(())
    }

    pub fn enqueue<T, F>(&self, job: F) -> impl Future<Output = DurableResult<T>> + Send + use<T, F>
    where
        T: Send + 'static,
        F: Future<Output = DurableResult<T>> + Send + 'static,
    {
        self.line.enqueue(job)
    }

    pub fn install(&self, document: LoadedDocument) {
        let address_id = document.address_id.clone();
        lock(&self.documents).insert(address_id.clone(), Arc::new(document));
        self.touch_document(&address_id);
    }

    /// Remove a retired incarnation if it is still the cached occupant of its address.
    pub fn evict(&self, address_id: &str, record_id: crate::ids::DocumentId) {
        let mut documents = lock(&self.documents);
        if documents.get(address_id).is_some_and(|loaded| loaded.record.id == record_id) {
            documents.remove(address_id);
        }
    }

    fn add_listener<L>(&self, list: &Mutex<Vec<(u64, L)>>, listener: L) -> u64 {
        let id = self.next_listener.fetch_add(1, Ordering::Relaxed);
        lock(list).push((id, listener));
        id
    }

    pub fn observe_commits(&self, listener: ObservedCommitListener) -> Unsubscribe {
        let id = self.add_listener(&self.internal_listeners, listener);
        let weak = self.weak.clone();
        Unsubscribe::new(move || {
            if let Some(core) = weak.upgrade() {
                lock(&core.internal_listeners).retain(|(other, _)| *other != id);
            }
        })
    }

    pub fn subscribe_close(&self, listener: CloseListener) -> Unsubscribe {
        let id = self.add_listener(&self.close_listeners, listener);
        let weak = self.weak.clone();
        Unsubscribe::new(move || {
            if let Some(core) = weak.upgrade() {
                lock(&core.close_listeners).retain(|(other, _)| *other != id);
            }
        })
    }

    pub fn publish(&self, seq: Seq, writes: &[StorageWrite], documents: AdoptedDocuments, context: &Context) {
        let telemetry = context.telemetry();
        for write in writes {
            match write {
                StorageWrite::Entry { value } => {
                    let attributes =
                        SpanAttributes::new().with("entry.id", value.id.get()).with("entry.kind", value.kind.as_str());
                    telemetry.add_event("pi.durable.entry", attributes);
                }
                StorageWrite::Submission { value } if value.status.is_settled() => {
                    let attributes = SpanAttributes::new()
                        .with("submission.id", value.id.get())
                        .with("status", value.status.as_str())
                        .with_optional("answer.entry_id", value.answer.map(EntryId::get));
                    telemetry.add_event("pi.durable.submission", attributes);
                }
                _ => {}
            }
        }
        let internal: Vec<_> = lock(&self.internal_listeners).iter().map(|(_, l)| l.clone()).collect();
        let host: Vec<_> = lock(&self.commit_listeners).iter().map(|(_, l)| l.clone()).collect();
        if internal.is_empty() && host.is_empty() {
            return;
        }
        let mut changes = Vec::with_capacity(writes.len() + documents.observed.len());
        for write in writes {
            match write {
                StorageWrite::Conversation { value } => changes.push(CommitChange::Conversation(*value)),
                StorageWrite::Entry { value } => changes.push(CommitChange::Entry(value.clone())),
                StorageWrite::Task { value } => changes.push(CommitChange::Task(value.clone())),
                StorageWrite::Submission { value } => changes.push(CommitChange::Submission(value.clone())),
                _ => {}
            }
        }
        // One publication for both, unless the commit also stored held changes, which only durable listeners get.
        let stored = documents.stored.map(|stored| {
            let tables = changes.iter().cloned();
            CommitPublication { seq, changes: tables.chain(stored.into_iter().map(CommitChange::Document)).collect() }
        });
        changes.extend(documents.observed.into_iter().map(CommitChange::Document));
        let observed = ObservedCommit::Stored(CommitPublication { seq, changes });
        // The commit is durable: a failing listener neither fails the commit nor skips the others.
        for listener in internal {
            if let Err(error) = listener(&observed, context) {
                self.fail_in(error, Some(context));
            }
        }
        let ObservedCommit::Stored(own) = &observed else { return };
        self.publish_stored(stored.as_ref().unwrap_or(own), &host, context);
    }

    /// Give `subscribe_commits()` listeners one stored commit; a failing listener is reported.
    pub(super) fn publish_stored(&self, publication: &CommitPublication, host: &[CommitListener], context: &Context) {
        for listener in host {
            if let Err(error) = listener(publication, context) {
                self.hooks.report(&error, &ReportInfo::ListenerError, Some(context));
            }
        }
    }

    pub(super) fn host_listeners(&self) -> Vec<CommitListener> {
        lock(&self.commit_listeners).iter().map(|(_, l)| l.clone()).collect()
    }

    pub(super) fn internal_listeners(&self) -> Vec<ObservedCommitListener> {
        lock(&self.internal_listeners).iter().map(|(_, l)| l.clone()).collect()
    }
}

impl Session {
    pub(crate) fn open(
        storage: Arc<dyn Storage>,
        hooks: Arc<dyn SessionHooks>,
        now: Arc<dyn Fn() -> f64 + Send + Sync>,
        settings: Settings,
    ) -> Self {
        let Settings { retention, persist_within, read_only, set_timer } = settings;
        let reporter = hooks.clone();
        let report = Arc::new(move |error: &DurableError| {
            let info = ReportInfo::HostError { callback: HostCallback::SetTimer, task: None };
            reporter.report(error, &info, None);
        });
        let timers = Timers::new(set_timer, report);
        let core = Arc::new_cyclic(|weak: &Weak<SessionCore>| {
            let latch: Weak<dyn FailureLatch> = weak.clone();
            SessionCore {
                storage: Arc::new(GuardedStorage::new(storage, latch)),
                documents: Mutex::default(),
                commit_listeners: Mutex::default(),
                internal_listeners: Mutex::default(),
                close_listeners: Mutex::default(),
                next_listener: AtomicU64::new(1),
                line: Line::new(),
                closing: Mutex::new(None),
                failure: Mutex::new(None),
                failed: watch::channel(None).0,
                closed: watch::channel(None).0,
                hooks,
                now,
                retention,
                persist_within,
                holds: Mutex::default(),
                timers,
                read_only,
                weak: weak.clone(),
            }
        });
        Self(core)
    }

    /// The Storage behind the failure guard; every component reads and writes through it.
    pub fn storage(&self) -> Arc<dyn Storage> {
        self.0.storage.clone()
    }

    /// Run `change` on the mutation line and commit what it staged atomically.
    pub fn commit<T, F, Fut>(
        &self,
        change: F,
        context: &Context,
    ) -> impl Future<Output = DurableResult<T>> + Send + use<T, F, Fut>
    where
        T: Send + 'static,
        F: FnOnce(Tx) -> Fut + Send + 'static,
        Fut: Future<Output = DurableResult<T>> + Send + 'static,
    {
        self.commit_scoped(change, context, TransactionScope::default(), CommitOptions::default())
    }

    /// [`Self::commit`] with options: `options.persistence` may defer storing it ([`crate::CommitPersistence`]).
    pub fn commit_with_options<T, F, Fut>(
        &self,
        change: F,
        context: &Context,
        options: CommitOptions,
    ) -> impl Future<Output = DurableResult<T>> + Send + use<T, F, Fut>
    where
        T: Send + 'static,
        F: FnOnce(Tx) -> Fut + Send + 'static,
        Fut: Future<Output = DurableResult<T>> + Send + 'static,
    {
        self.commit_scoped(change, context, TransactionScope::default(), options)
    }

    /// Internal commit with a scope: the default `create_task` conversation and the task stamped on entries.
    pub fn commit_with<T, F, Fut>(
        &self,
        change: F,
        context: &Context,
        scope: TransactionScope,
    ) -> impl Future<Output = DurableResult<T>> + Send + use<T, F, Fut>
    where
        T: Send + 'static,
        F: FnOnce(Tx) -> Fut + Send + 'static,
        Fut: Future<Output = DurableResult<T>> + Send + 'static,
    {
        self.commit_scoped(change, context, scope, CommitOptions::default())
    }

    /// Internal: [`Self::commit_with`] with options.
    pub fn commit_scoped<T, F, Fut>(
        &self,
        change: F,
        context: &Context,
        scope: TransactionScope,
        options: CommitOptions,
    ) -> impl Future<Output = DurableResult<T>> + Send + use<T, F, Fut>
    where
        T: Send + 'static,
        F: FnOnce(Tx) -> Fut + Send + 'static,
        Fut: Future<Output = DurableResult<T>> + Send + 'static,
    {
        let usable = self.0.assert_usable().and_then(|()| deferral(&self.0, options.persistence));
        let core = self.0.clone();
        let context = context.clone();
        let run = usable.map(|within| self.0.enqueue(run_commit(core, change, context, scope, within)));
        async move { run?.await }
    }

    /// Internal: run a read-only job on the mutation line so multi-read derivations see one committed state.
    pub fn read_on_line<T, F>(&self, job: F) -> impl Future<Output = DurableResult<T>> + Send + use<T, F>
    where
        T: Send + 'static,
        F: Future<Output = DurableResult<T>> + Send + 'static,
    {
        let usable = self.0.assert_usable();
        let core = self.0.clone();
        let run = usable.map(|()| {
            self.0.enqueue(async move {
                core.assert_healthy()?;
                job.await
            })
        });
        async move { run?.await }
    }

    /// Internal: fail the Session with `error`; the first wins. Admission ends and close listeners run now.
    pub fn fail(&self, error: DurableError, context: Option<&Context>) {
        self.0.fail_in(error, context);
    }

    /// Internal: the error that failed the Session, if one did.
    pub fn failure(&self) -> Option<DurableError> {
        self.0.failure()
    }

    /// Internal: settles with `SessionFailed` the moment the Session fails; pending forever otherwise.
    pub fn failed(&self) -> impl Future<Output = DurableError> + Send + use<> {
        let mut receiver = self.0.failed.subscribe();
        async move {
            let error = receiver.wait_for(Option::is_some).await.ok().and_then(|error| error.clone());
            match error {
                Some(error) => DurableError::session_failed(error),
                None => std::future::pending().await,
            }
        }
    }

    /// Settles once closed, Storage included.
    pub fn closed(&self) -> impl Future<Output = SessionEnd> + Send + use<> {
        let mut receiver = self.0.closed.subscribe();
        async move {
            let end = receiver.wait_for(Option::is_some).await.ok().and_then(|end| end.clone());
            match end {
                Some(end) => end,
                None => std::future::pending().await,
            }
        }
    }

    /// Internal: `SessionFailed` when failed, else an error when closing.
    pub fn assert_usable(&self) -> DurableResult<()> {
        self.0.assert_usable()
    }

    pub fn report(&self, error: &DurableError) {
        self.0.report(error);
    }

    /// Internal, for `SessionHooks::before_close()`: on the line, after every commit admitted before the seal has
    /// settled, write what `writes()` returns then straight to Storage. Nothing is published, since the Session is
    /// closing; a failed Session writes nothing.
    pub(crate) async fn write_at_close(
        &self,
        writes: impl FnOnce() -> Vec<StorageWrite> + Send + 'static,
    ) -> DurableResult<()> {
        let core = self.0.clone();
        self.0
            .enqueue(async move {
                if core.failure().is_some() {
                    return Ok(());
                }
                let batch = writes();
                if !batch.is_empty() {
                    core.storage.commit(&batch, &|_| {}, &BACKGROUND_CONTEXT).await?;
                }
                Ok(())
            })
            .await
    }

    /// Seal admission, run close listeners, settle admitted commits, then close Storage.
    pub fn close(&self, context: &Context) -> impl Future<Output = DurableResult<()>> + Send + use<> {
        let (closing, created) = {
            let mut slot = lock(&self.0.closing);
            match &*slot {
                Some(closing) => (closing.clone(), false),
                None => {
                    let closing = spawn_shared(close_session(self.0.clone(), without_abort_signal(context)));
                    *slot = Some(closing.clone());
                    (closing, true)
                }
            }
        };
        if created {
            self.0.cancel_document_sweep();
            self.0.disarm_hold_timer();
            let listeners: Vec<_> = std::mem::take(&mut *lock(&self.0.close_listeners));
            for (_, listener) in listeners {
                listener();
            }
        }
        let context = context.clone();
        async move { await_with_context(closing, &context).await }
    }

    /// Register a synchronous post-adoption listener. An error it returns is reported.
    pub fn subscribe_commits(
        &self,
        listener: impl Fn(&CommitPublication, &Context) -> DurableResult<()> + Send + Sync + 'static,
    ) -> DurableResult<Unsubscribe> {
        self.0.assert_usable()?;
        let id = self.0.add_listener(&self.0.commit_listeners, Arc::new(listener) as CommitListener);
        let weak = Arc::downgrade(&self.0);
        Ok(Unsubscribe::new(move || {
            if let Some(core) = weak.upgrade() {
                lock(&core.commit_listeners).retain(|(other, _)| *other != id);
            }
        }))
    }

    /// Internal: `subscribe_commits` for the Session's own components; an error fails the Session.
    pub fn observe_commits(
        &self,
        listener: impl Fn(&ObservedCommit, &Context) -> DurableResult<()> + Send + Sync + 'static,
    ) -> DurableResult<Unsubscribe> {
        self.0.assert_usable()?;
        Ok(self.0.observe_commits(Arc::new(listener)))
    }

    /// Register a listener called synchronously when close begins, also when a failure closes the Session.
    pub fn subscribe_close(&self, listener: impl Fn() + Send + Sync + 'static) -> DurableResult<Unsubscribe> {
        self.0.assert_usable()?;
        Ok(self.0.subscribe_close(Arc::new(listener)))
    }

    /// Internal, for tests: a weak reference that upgrades while anything keeps the Session kernel alive.
    #[doc(hidden)]
    pub fn downgrade(&self) -> Weak<dyn std::any::Any + Send + Sync> {
        let weak: Weak<SessionCore> = Arc::downgrade(&self.0);
        weak
    }

    /// Internal, for tests: how many documents the cache holds, how many are idle or pinned, and the sweep jobs run.
    #[doc(hidden)]
    pub fn document_cache_sizes(&self) -> DocumentCacheSizes {
        let sizes = lock(&self.0.documents).sizes();
        DocumentCacheSizes { held: lock(&self.0.holds).len(), ..sizes }
    }

    /// Drop every loaded tracker on the mutation line, held changes stored first; later access cold-loads from
    /// Storage.
    pub fn unload_documents(&self) -> impl Future<Output = DurableResult<()>> + Send + use<> {
        let core = self.0.clone();
        self.0.enqueue(async move {
            if core.failure().is_none() {
                core.store_all_holds(&BACKGROUND_CONTEXT).await?;
            }
            lock(&core.documents).clear();
            Ok(())
        })
    }
}

async fn close_session(core: Arc<SessionCore>, cleanup: Context) -> DurableResult<()> {
    core.hooks.before_close().await;
    let job_core = core.clone();
    let result = core
        .enqueue(async move {
            // Last, after the writes at close, while the trackers are still here. A failed Session writes nothing; a
            // failing store fails it, and the backend still closes.
            let unstored = match job_core.failure() {
                None => job_core.store_all_holds(&cleanup).await.err(),
                Some(_) => None,
            };
            lock(&job_core.commit_listeners).clear();
            lock(&job_core.internal_listeners).clear();
            lock(&job_core.documents).clear_all();
            job_core.clear_holds();
            let result = job_core.storage.close(&cleanup).await;
            if let Err(error) = &result {
                // A Storage that cannot close is a failed one; an earlier failure stays the cause.
                let first = {
                    let mut failure = lock(&job_core.failure);
                    let first = failure.is_none();
                    if first {
                        *failure = Some(error.clone());
                    }
                    first
                };
                if first {
                    job_core.hooks.report(error, &ReportInfo::SessionFailed, None);
                }
            }
            match unstored {
                Some(error) => result.and(Err(error)),
                None => result,
            }
        })
        .await;
    let end = match core.failure() {
        Some(error) => SessionEnd::Failed(error),
        None => SessionEnd::Closed,
    };
    core.closed.send_replace(Some(end));
    result
}

async fn run_commit<T, F, Fut>(
    core: Arc<SessionCore>,
    change: F,
    context: Context,
    scope: TransactionScope,
    within: Option<f64>,
) -> DurableResult<T>
where
    F: FnOnce(Tx) -> Fut,
    Fut: Future<Output = DurableResult<T>>,
{
    core.assert_healthy()?;
    if let Some(signal) = context.abort_signal() {
        signal.throw_if_aborted()?;
    }
    let tx = Tx::new(core.clone(), context.clone(), scope);
    let result = commit_tx(&core, &tx, change, context, within).await;
    tx.release();
    result
}

async fn commit_tx<T, F, Fut>(
    core: &Arc<SessionCore>,
    tx: &Tx,
    change: F,
    context: Context,
    within: Option<f64>,
) -> DurableResult<T>
where
    F: FnOnce(Tx) -> Fut,
    Fut: Future<Output = DurableResult<T>>,
{
    // A callback that caught a failed read must not commit, nor succeed as if it had.
    let result = change(tx.clone()).await.and_then(|value| core.assert_healthy().map(|()| value));
    let value = match result {
        Ok(value) => value,
        Err(error) => {
            tx.settle_failure().await;
            core.sweep_documents();
            return Err(error);
        }
    };
    let mut writes = match tx.settle_success().await {
        Ok(writes) => writes,
        Err(error) => {
            // Settlement aborted every change already.
            core.sweep_documents();
            return Err(error);
        }
    };
    if writes.is_empty() {
        tx.discard();
        core.sweep_documents();
        return Ok(value);
    }
    if core.read_only {
        // Refused before Storage: the Session stays healthy, and the caller sees why.
        tx.discard();
        core.sweep_documents();
        return Err(DurableError::ReadOnlySession("a commit with writes".into()));
    }
    if let Some(within) = within
        && let Some(held) = tx.holdable(&writes)
    {
        core.hold(tx, held, within, &context);
        core.sweep_documents();
        return Ok(value);
    }
    // The last step before Storage: held changes leave their queue only once nothing but Storage can fail.
    let taken = tx.take_held(&mut writes);
    // Storage adopts the batch synchronously once it is durable, before its next commit; the callback keeps its own
    // error, so it never reaches Storage.
    let adoption: Mutex<Option<DurableResult<AdoptedDocuments>>> = Mutex::new(None);
    let adopt_tx = tx.clone();
    let adopt = |committed: Seq| {
        let adopted = adopt_tx.adopt(committed);
        *lock(&adoption) = Some(adopted);
    };
    // Once admitted, caller cancellation does not interrupt Storage settlement.
    let committed = core.storage.commit(&writes, &adopt, &without_abort_signal(&context)).await;
    let adoption = lock(&adoption).take();
    let seq = match committed {
        Ok(seq) => seq,
        Err(error) => {
            // The guard has failed the Session. A rejection after adoption leaves nothing staged to discard.
            if adoption.is_none() {
                tx.discard();
            }
            return Err(error);
        }
    };
    let documents = match adoption {
        Some(Ok(documents)) => documents,
        Some(Err(error)) => {
            // Storage already committed; a failed adoption leaves memory behind durable state.
            core.fail(error.clone());
            return Err(error);
        }
        None => {
            let error = DurableError::error("Storage committed without adopting the batch");
            core.fail(error.clone());
            return Err(error);
        }
    };
    if !taken.is_empty() {
        core.release_holds(&taken);
    }
    core.publish(seq, &writes, documents, &context);
    core.sweep_documents();
    Ok(value)
}
