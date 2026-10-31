//! Document observers: watches, document states, and host observers that follow one document address (spec sections
//! 9.1 and 9.2).

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::delta::Op;
use pi_chord::state::{ReplicatedStateSourceOptions, attach_replicated_state_source};
use pi_chord::{Context, Json, Unsubscribe, without_abort_signal};

use super::cache::DocumentPin;
use super::core::{LoadedDocument, Session, SessionCore};
use super::guarded::FailureLatch;
use super::line::lock;
use super::observation::{CommittedObserver, CommittedStateSource, Release};
use super::observer::{GuardedObserver, Observation, Observer};
use super::reads::{Def, check_loaded};
use super::watch::DocumentWatch;
use super::{DocumentState, WatchCall, session_end};
use crate::documents::{DocFamilyToken, DocScope, DocToken, DocValue, ResolvedAddress};
use crate::errors::{DurableError, DurableResult};
use crate::ids::DocumentId;
use crate::types::{CommitChange, DocumentAddress, DocumentCommitChange, DocumentRecord, DocumentScope};

fn cancellation(context: &Context) -> DurableError {
    match context.abort_signal().and_then(|signal| signal.reason()) {
        Some(reason) => reason.into(),
        None => DurableError::aborted(),
    }
}

/// Whether `record` is an incarnation at `address`.
fn at_address(record: &DocumentRecord, address: &DocumentAddress) -> bool {
    record.kind == address.kind && record.key == address.key && record.scope == address.scope
}

/// What the observer of one address follows: the incarnation there, its value, and the definition version of that
/// value; none while the address is absent, so the Session keeps no retired value.
struct Followed {
    incarnation: Option<DocumentId>,
    value: Option<Json>,
    version: u32,
    pin: Option<Arc<DocumentPin>>,
}

impl SessionCore {
    /// Follow the address of the incarnation `loaded` with `observer`: each committed change there, a retirement as a
    /// `retired` change with the last value and no ops, and a later incarnation as a root replacement. When a commit
    /// makes the task of a task document terminal, the observer's owner ended. The current incarnation is pinned.
    pub(crate) fn attach_observer(
        self: &Arc<Self>,
        loaded: &LoadedDocument,
        observer: Arc<dyn CommittedObserver>,
        release: &Arc<Release>,
    ) -> DurableResult<()> {
        // As upstream `observeCommits`, attaching fails once close began.
        self.assert_usable()?;
        let address = loaded.record.address();
        let address_id = loaded.address_id.clone();
        let followed = Arc::new(Mutex::new(Followed {
            incarnation: Some(loaded.record.id),
            value: Some(loaded.value()),
            version: loaded.value_version,
            pin: Some(self.pin_document(&address_id, loaded.record.id)),
        }));
        let attached = Arc::new(AtomicBool::new(true));
        let commits: Arc<Mutex<Option<Unsubscribe>>> = Arc::default();
        let (target, state, alive, subscription) =
            (observer.clone(), followed.clone(), attached.clone(), commits.clone());
        let weak = Arc::downgrade(self);
        let listener = self.follow_commits(Arc::new(move |publication, context| {
            // Listeners run from a copy of the list: another observer's call may have detached this one.
            if !alive.load(Ordering::SeqCst) {
                return Ok(());
            }
            let context = if target.keeps_cancellation() { context.clone() } else { without_abort_signal(context) };
            let mut owner_ends = false;
            for change in publication.changes() {
                let delivery = match change {
                    CommitChange::Task(record) => {
                        let ends = matches!(address.scope, DocumentScope::Task { task_id } if task_id == record.id);
                        owner_ends |= ends && record.is_terminal();
                        continue;
                    }
                    CommitChange::Document(DocumentCommitChange::Change { record, version, value, ops, .. }) => {
                        let Some(core) = weak.upgrade() else { return Ok(()) };
                        follow(&core, &state, &address, &address_id, record, Some((*version, value)), ops)
                    }
                    CommitChange::Document(DocumentCommitChange::Retire { record, .. }) => {
                        let Some(core) = weak.upgrade() else { return Ok(()) };
                        follow(&core, &state, &address, &address_id, record, None, &[])
                    }
                    _ => continue,
                };
                let Some(delivery) = delivery else { continue };
                target.advance(delivery, context.clone());
                // The observer may detach in its call, or fail and be detached: the rest of the commit is not its.
                if !alive.load(Ordering::SeqCst) {
                    return Ok(());
                }
            }
            if owner_ends {
                if let Some(unsubscribe) = lock(&subscription).take() {
                    unsubscribe.unsubscribe();
                }
                if let Some(pin) = lock(&state).pin.take() {
                    pin.release();
                }
                target.owner_ended();
            }
            Ok(())
        }));
        *lock(&commits) = Some(listener);
        let (detach_state, detach_alive) = (followed.clone(), attached.clone());
        let detach_commits = commits.clone();
        release.push(move || {
            detach_alive.store(false, Ordering::SeqCst);
            if let Some(unsubscribe) = lock(&detach_commits).take() {
                unsubscribe.unsubscribe();
            }
            if let Some(pin) = lock(&detach_state).pin.take() {
                pin.release();
            }
        });
        // The detach runs before `end`, so nothing of a commit underway reaches the observer after it.
        let (weak, detach) = (Arc::downgrade(self), release.clone());
        let close = self.subscribe_close(Arc::new(move || {
            detach.run();
            observer.end(session_end(weak.upgrade().and_then(|core| core.failure())));
        }));
        release.push(move || close.unsubscribe());
        Ok(())
    }
}

/// One committed document change seen by the observer of `address`: the call it delivers, if any, after updating
/// what it follows. `content` is the version and value of a change, `None` for a retirement.
fn follow(
    core: &Arc<SessionCore>,
    state: &Mutex<Followed>,
    address: &DocumentAddress,
    address_id: &str,
    record: &DocumentRecord,
    content: Option<(u32, &Json)>,
    ops: &[Op],
) -> Option<WatchCall> {
    let mut followed = lock(state);
    // The incarnation followed is at the address by definition; only another record needs the address compared.
    if followed.incarnation != Some(record.id) && !at_address(record, address) {
        return None;
    }
    let Some((version, value)) = content else {
        if followed.incarnation != Some(record.id) {
            return None;
        }
        // A retired incarnation never changes again: the observer gets the retirement but pins nothing.
        let last = followed.value.take().unwrap_or_default();
        followed.incarnation = None;
        if let Some(pin) = followed.pin.take() {
            pin.release();
        }
        return Some(WatchCall::Value { value: last, ops: Vec::new(), retired: true });
    };
    if followed.incarnation != Some(record.id) || version != followed.version {
        // A new incarnation at the address, or a value of another definition version: shown whole.
        if followed.incarnation != Some(record.id) {
            followed.incarnation = Some(record.id);
            if let Some(pin) = followed.pin.take() {
                pin.release();
            }
            followed.pin = Some(core.pin_document(address_id, record.id));
        }
        followed.version = version;
        followed.value = Some(value.clone());
        return Some(WatchCall::new(value.clone(), vec![Op::Replace(value.clone())]));
    }
    // A migration-only base changes nothing for an observer of the new version.
    if ops.is_empty() {
        return None;
    }
    followed.value = Some(value.clone());
    Some(WatchCall::new(value.clone(), ops.to_vec()))
}

impl Session {
    /// A hydrated read-only state of the singleton for `owner`, or `None` when absent. Never creates.
    pub fn document_state<T: DocValue, S: DocScope>(
        &self,
        token: &DocToken<T, S>,
        owner: S::Owner,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<DocumentState>>> + Send + use<T, S> {
        self.document_state_erased(token.definition().clone(), token.resolve(owner), context)
    }

    pub fn family_document_state<T: DocValue, I: DocValue, S: DocScope>(
        &self,
        token: &DocFamilyToken<T, I, S>,
        owner: S::Owner,
        key: &str,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<DocumentState>>> + Send + use<T, I, S> {
        self.document_state_erased(token.definition().clone(), token.resolve(owner, key), context)
    }

    pub fn document_state_erased(
        &self,
        definition: Def,
        resolved: ResolvedAddress,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<DocumentState>>> + Send + use<> {
        let usable = self.0.assert_usable();
        let core = self.0.clone();
        let context = context.clone();
        let run = usable.map(|()| {
            self.0.enqueue(async move {
                core.assert_healthy()?;
                let loaded = core.load_document(&definition, &resolved.id, &resolved.address, &context).await?;
                let Some(loaded) = loaded else { return Ok(None) };
                check_loaded(&definition, &loaded)?;
                let release = Release::new();
                let source = CommittedStateSource::new(loaded.value(), release.clone());
                core.attach_observer(&loaded, source.clone(), &release)?;
                let report = core.clone();
                let on_error = Arc::new(move |error: pi_chord::ChordError| report.report(&error.into()));
                let options = ReplicatedStateSourceOptions { on_error: Some(on_error) };
                match attach_replicated_state_source(source.as_ref(), options) {
                    Ok(state) => Ok(Some(state)),
                    Err(error) => {
                        release.run();
                        Err(error.into())
                    }
                }
            })
        });
        async move { run?.await }
    }

    /// A watch of the singleton's address for `owner`, or `None` when absent. The context governs the watch lifetime.
    pub fn watch_doc<T: DocValue, S: DocScope>(
        &self,
        token: &DocToken<T, S>,
        owner: S::Owner,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<DocumentWatch>>> + Send + use<T, S> {
        self.watch_doc_erased(token.definition().clone(), token.resolve(owner), context)
    }

    pub fn family_watch_doc<T: DocValue, I: DocValue, S: DocScope>(
        &self,
        token: &DocFamilyToken<T, I, S>,
        owner: S::Owner,
        key: &str,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<DocumentWatch>>> + Send + use<T, I, S> {
        self.watch_doc_erased(token.definition().clone(), token.resolve(owner, key), context)
    }

    pub fn watch_doc_erased(
        &self,
        definition: Def,
        resolved: ResolvedAddress,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<DocumentWatch>>> + Send + use<> {
        let attach = |core: &Arc<SessionCore>, loaded: &LoadedDocument| {
            let release = Release::new();
            let (report, detach) = (core.clone(), release.clone());
            let watch = DocumentWatch::new(loaded.value(), move || detach.run(), move |e| report.report(e));
            core.attach_observer(loaded, Arc::new(watch.clone()), &release)?;
            Ok(watch)
        };
        let attached = self.attach_document(definition, resolved, context, attach);
        let context = context.clone();
        async move {
            let Some(watch) = attached.await? else { return Ok(None) };
            if context.is_cancelled() {
                watch.abort();
                return Err(cancellation(&context));
            }
            if let Some(signal) = context.abort_signal() {
                watch.observe_abort(&signal)?;
            }
            Ok(Some(watch))
        }
    }

    /// Attach a synchronous [`Observer`] to the singleton's address for `owner`, atomically on the Session line, as
    /// `watch_doc()` acquires a watch; `None` when absent. `context` governs only the attachment.
    pub fn observe_doc<T: DocValue, S: DocScope>(
        &self,
        token: &DocToken<T, S>,
        owner: S::Owner,
        observer: Arc<dyn Observer>,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<Observation>>> + Send + use<T, S> {
        self.observe_doc_erased(token.definition().clone(), token.resolve(owner), observer, context)
    }

    pub fn family_observe_doc<T: DocValue, I: DocValue, S: DocScope>(
        &self,
        token: &DocFamilyToken<T, I, S>,
        owner: S::Owner,
        key: &str,
        observer: Arc<dyn Observer>,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<Observation>>> + Send + use<T, I, S> {
        self.observe_doc_erased(token.definition().clone(), token.resolve(owner, key), observer, context)
    }

    pub fn observe_doc_erased(
        &self,
        definition: Def,
        resolved: ResolvedAddress,
        observer: Arc<dyn Observer>,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<Observation>>> + Send + use<> {
        let attach = move |core: &Arc<SessionCore>, loaded: &LoadedDocument| {
            let release = Release::new();
            let (report, detach) = (core.clone(), release.clone());
            let guarded = GuardedObserver::new(observer, move || detach.run(), move |e| report.report(e));
            core.attach_observer(loaded, guarded.clone(), &release)?;
            Ok(Observation::new(loaded.value(), Arc::new(move || guarded.detach())))
        };
        let attached = self.attach_document(definition, resolved, context, attach);
        let context = context.clone();
        async move {
            let Some(observation) = attached.await? else { return Ok(None) };
            if context.is_cancelled() {
                observation.detach();
                return Err(cancellation(&context));
            }
            Ok(Some(observation))
        }
    }

    /// On the line, as one job: load the document at `resolved` and `attach` to it, so the attachment sees every
    /// commit after the value it starts from; `None` when absent.
    fn attach_document<A, F>(
        &self,
        definition: Def,
        resolved: ResolvedAddress,
        context: &Context,
        attach: F,
    ) -> impl Future<Output = DurableResult<Option<A>>> + Send + use<A, F>
    where
        A: Send + 'static,
        F: FnOnce(&Arc<SessionCore>, &LoadedDocument) -> DurableResult<A> + Send + 'static,
    {
        let usable = self.0.assert_usable();
        let core = self.0.clone();
        let context = context.clone();
        let run = usable.map(|()| {
            self.0.enqueue(async move {
                core.assert_healthy()?;
                if context.is_cancelled() {
                    return Err(cancellation(&context));
                }
                let loaded = core.load_document(&definition, &resolved.id, &resolved.address, &context).await?;
                if context.is_cancelled() {
                    return Err(cancellation(&context));
                }
                let Some(loaded) = loaded else { return Ok(None) };
                check_loaded(&definition, &loaded)?;
                attach(&core, &loaded).map(Some)
            })
        });
        async move { run?.await }
    }
}
