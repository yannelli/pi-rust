//! Session kernel: the mutation line, the loaded document tracker cache, commit, close, and failure.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use pi_chord::delta::Tracker;
use pi_chord::{BACKGROUND_CONTEXT, Context, Json, Unsubscribe, without_abort_signal};
use tokio::sync::watch;

use super::guarded::{FailureLatch, GuardedStorage};
use super::line::{Line, SharedResult, await_with_context, lock, spawn_shared};
use super::tx::{TransactionScope, Tx};
use super::{CommitListener, SessionEnd, SessionHooks};
use crate::errors::{DurableError, DurableResult};
use crate::ids::Seq;
use crate::types::{CommitChange, CommitPublication, DocumentCommitChange, DocumentRecord, Storage, StorageWrite};

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

pub(crate) struct SessionCore {
    pub storage: Arc<GuardedStorage>,
    pub documents: Mutex<HashMap<String, Arc<LoadedDocument>>>,
    commit_listeners: Mutex<Vec<(u64, CommitListener)>>,
    /// The Session's own listeners: they keep memory in step with storage, so a failure fails it.
    internal_listeners: Mutex<Vec<(u64, CommitListener)>>,
    close_listeners: Mutex<Vec<(u64, CloseListener)>>,
    next_listener: AtomicU64,
    line: Line,
    closing: Mutex<Option<SharedResult<()>>>,
    failure: Mutex<Option<DurableError>>,
    failed: watch::Sender<Option<DurableError>>,
    closed: watch::Sender<Option<SessionEnd>>,
    pub hooks: Arc<dyn SessionHooks>,
    pub now: Arc<dyn Fn() -> f64 + Send + Sync>,
    weak: Weak<SessionCore>,
}

/// Session kernel handle; cheap to clone. Only committed state is observable.
#[derive(Clone)]
pub struct Session(pub(crate) Arc<SessionCore>);

impl FailureLatch for SessionCore {
    fn failure(&self) -> Option<DurableError> {
        lock(&self.failure).clone()
    }

    fn fail(&self, error: DurableError) {
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
        self.report(&error);
    }
}

impl SessionCore {
    pub fn report(&self, error: &DurableError) {
        self.hooks.report(error);
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

    pub fn enqueue<T, F>(&self, job: F) -> impl Future<Output = DurableResult<T>> + Send + 'static
    where
        T: Send + 'static,
        F: Future<Output = DurableResult<T>> + Send + 'static,
    {
        self.line.enqueue(job)
    }

    pub fn install(&self, document: LoadedDocument) {
        lock(&self.documents).insert(document.address_id.clone(), Arc::new(document));
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

    pub fn observe_commits(&self, listener: CommitListener) -> Unsubscribe {
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

    pub fn publish(&self, seq: Seq, writes: &[StorageWrite], documents: Vec<DocumentCommitChange>, context: &Context) {
        let internal: Vec<_> = lock(&self.internal_listeners).iter().map(|(_, l)| l.clone()).collect();
        let host: Vec<_> = lock(&self.commit_listeners).iter().map(|(_, l)| l.clone()).collect();
        if internal.is_empty() && host.is_empty() {
            return;
        }
        let mut changes = Vec::with_capacity(writes.len() + documents.len());
        for write in writes {
            match write {
                StorageWrite::Conversation { value } => changes.push(CommitChange::Conversation(*value)),
                StorageWrite::Entry { value } => changes.push(CommitChange::Entry(value.clone())),
                StorageWrite::Task { value } => changes.push(CommitChange::Task(value.clone())),
                StorageWrite::Submission { value } => changes.push(CommitChange::Submission(value.clone())),
                _ => {}
            }
        }
        changes.extend(documents.into_iter().map(CommitChange::Document));
        let publication = CommitPublication { seq, changes };
        // The commit is durable: a failing listener neither fails the commit nor skips the others.
        for listener in internal {
            if let Err(error) = listener(&publication, context) {
                self.fail(error);
            }
        }
        for listener in host {
            if let Err(error) = listener(&publication, context) {
                self.report(&error);
            }
        }
    }
}

impl Session {
    pub(crate) fn open(
        storage: Arc<dyn Storage>,
        hooks: Arc<dyn SessionHooks>,
        now: Arc<dyn Fn() -> f64 + Send + Sync>,
    ) -> Self {
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
    ) -> impl Future<Output = DurableResult<T>> + Send + 'static
    where
        T: Send + 'static,
        F: FnOnce(Tx) -> Fut + Send + 'static,
        Fut: Future<Output = DurableResult<T>> + Send + 'static,
    {
        self.commit_with(change, context, TransactionScope::default())
    }

    /// Internal commit with a scope: the default `create_task` conversation and the task stamped on entries.
    pub fn commit_with<T, F, Fut>(
        &self,
        change: F,
        context: &Context,
        scope: TransactionScope,
    ) -> impl Future<Output = DurableResult<T>> + Send + 'static
    where
        T: Send + 'static,
        F: FnOnce(Tx) -> Fut + Send + 'static,
        Fut: Future<Output = DurableResult<T>> + Send + 'static,
    {
        let usable = self.0.assert_usable();
        let core = self.0.clone();
        let context = context.clone();
        let run = usable.map(|()| self.0.enqueue(run_commit(core, change, context, scope)));
        async move { run?.await }
    }

    /// Internal: run a read-only job on the mutation line so multi-read derivations see one committed state.
    pub fn read_on_line<T, F>(&self, job: F) -> impl Future<Output = DurableResult<T>> + Send + 'static
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
    pub fn fail(&self, error: DurableError) {
        self.0.fail(error);
    }

    /// Internal: the error that failed the Session, if one did.
    pub fn failure(&self) -> Option<DurableError> {
        self.0.failure()
    }

    /// Internal: settles with `SessionFailed` the moment the Session fails; pending forever otherwise.
    pub fn failed(&self) -> impl Future<Output = DurableError> + Send + 'static {
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
    pub fn closed(&self) -> impl Future<Output = SessionEnd> + Send + 'static {
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

    /// Seal admission, run close listeners, settle admitted commits, then close Storage.
    pub fn close(&self, context: &Context) -> impl Future<Output = DurableResult<()>> + Send + 'static {
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
        listener: impl Fn(&CommitPublication, &Context) -> DurableResult<()> + Send + Sync + 'static,
    ) -> DurableResult<Unsubscribe> {
        self.0.assert_usable()?;
        Ok(self.0.observe_commits(Arc::new(listener)))
    }

    /// Register a listener called synchronously when close begins, also when a failure closes the Session.
    pub fn subscribe_close(&self, listener: impl Fn() + Send + Sync + 'static) -> DurableResult<Unsubscribe> {
        self.0.assert_usable()?;
        Ok(self.0.subscribe_close(Arc::new(listener)))
    }

    /// Drop every loaded tracker on the mutation line; later access cold-loads from Storage.
    pub fn unload_documents(&self) -> impl Future<Output = DurableResult<()>> + Send + 'static {
        let core = self.0.clone();
        self.0.enqueue(async move {
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
            lock(&job_core.commit_listeners).clear();
            lock(&job_core.internal_listeners).clear();
            lock(&job_core.documents).clear();
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
                    job_core.report(error);
                }
            }
            result
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
    // A callback that caught a failed read must not commit, nor succeed as if it had.
    let result = change(tx.clone()).await.and_then(|value| core.assert_healthy().map(|()| value));
    let value = match result {
        Ok(value) => value,
        Err(error) => {
            tx.settle_failure().await;
            return Err(error);
        }
    };
    let writes = tx.settle_success().await?;
    if writes.is_empty() {
        tx.discard();
        return Ok(value);
    }
    // Once admitted, caller cancellation does not interrupt Storage settlement.
    let seq = match core.storage.commit(&writes, &without_abort_signal(&context)).await {
        Ok(seq) => seq,
        Err(error) => {
            tx.discard();
            return Err(error);
        }
    };
    let documents = match tx.adopt(seq) {
        Ok(documents) => documents,
        Err(error) => {
            // Storage already committed; a failed adoption leaves memory behind durable state.
            core.fail(error.clone());
            return Err(error);
        }
    };
    core.publish(seq, &writes, documents, &context);
    Ok(value)
}
