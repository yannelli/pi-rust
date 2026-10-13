//! Document states and watches: attach observers to one committed incarnation (spec sections 9.1 and 9.2).

use std::future::Future;
use std::sync::{Arc, Mutex};

use pi_chord::delta::Op;
use pi_chord::state::{ReplicatedStateSourceOptions, attach_replicated_state_source};
use pi_chord::{Context, Json, without_abort_signal};

use super::DocumentState;
use super::core::{LoadedDocument, Session, SessionCore};
use super::guarded::FailureLatch;
use super::line::lock;
use super::observation::{CommittedObserver, CommittedStateSource, Release, retirement_operations};
use super::reads::{Def, check_loaded};
use super::watch::CommittedWatch;
use crate::documents::{DocFamilyToken, DocScope, DocToken, DocValue, ResolvedAddress};
use crate::errors::{DurableError, DurableResult};
use crate::types::{CommitChange, DocumentCommitChange};

/// Operations an observer applies for one change. An observer hydrated under another definition version holds a
/// differently shaped value, so it receives the new value as a root replacement instead.
fn observed_operations(observed: &Mutex<u32>, version: Option<u32>, value: Option<&Json>, ops: &[Op]) -> Vec<Op> {
    let (Some(version), Some(value)) = (version, value) else { return retirement_operations() };
    let mut observed = lock(observed);
    if *observed == version {
        return ops.to_vec();
    }
    *observed = version;
    vec![Op::Replace(value.clone())]
}

fn cancellation(context: &Context) -> DurableError {
    match context.abort_signal().and_then(|signal| signal.reason()) {
        Some(reason) => reason.into(),
        None => DurableError::aborted(),
    }
}

impl SessionCore {
    /// Forward one incarnation's committed changes and the Session's close to `observer`.
    pub(crate) fn attach_observer(
        self: &Arc<Self>,
        loaded: &LoadedDocument,
        observer: Arc<dyn CommittedObserver>,
        release: &Release,
    ) {
        let record_id = loaded.record.id;
        let observed = Mutex::new(loaded.value_version);
        let target = observer.clone();
        let commits = self.observe_commits(Arc::new(move |publication, context| {
            for change in &publication.changes {
                let CommitChange::Document(DocumentCommitChange::Document { record, version, value, ops, .. }) = change
                else {
                    continue;
                };
                if record.id != record_id {
                    continue;
                }
                let ops = observed_operations(&observed, *version, value.as_ref(), ops);
                // A migration-only base changes nothing for an observer of the new version.
                if ops.is_empty() {
                    continue;
                }
                let context = if target.keeps_cancellation() { context.clone() } else { without_abort_signal(context) };
                target.advance(value.clone().unwrap_or(Json::Null), ops, context);
            }
            Ok(())
        }));
        let weak = Arc::downgrade(self);
        let close = self.subscribe_close(Arc::new(move || {
            observer.close_session(weak.upgrade().and_then(|core| core.failure()));
        }));
        release.push(move || {
            commits.unsubscribe();
            close.unsubscribe();
        });
    }
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
                core.attach_observer(&loaded, source.clone(), &release);
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

    /// A watch of the singleton for `owner`, or `None` when absent. The context governs the watch lifetime.
    pub fn watch_doc<T: DocValue, S: DocScope>(
        &self,
        token: &DocToken<T, S>,
        owner: S::Owner,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<CommittedWatch>>> + Send + use<T, S> {
        self.watch_doc_erased(token.definition().clone(), token.resolve(owner), context)
    }

    pub fn family_watch_doc<T: DocValue, I: DocValue, S: DocScope>(
        &self,
        token: &DocFamilyToken<T, I, S>,
        owner: S::Owner,
        key: &str,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<CommittedWatch>>> + Send + use<T, I, S> {
        self.watch_doc_erased(token.definition().clone(), token.resolve(owner, key), context)
    }

    pub fn watch_doc_erased(
        &self,
        definition: Def,
        resolved: ResolvedAddress,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<CommittedWatch>>> + Send + use<> {
        let usable = self.0.assert_usable();
        let core = self.0.clone();
        let context = context.clone();
        let line_context = context.clone();
        let run = usable.map(|()| {
            self.0.enqueue(async move {
                core.assert_healthy()?;
                if line_context.is_cancelled() {
                    return Err(cancellation(&line_context));
                }
                let loaded = core.load_document(&definition, &resolved.id, &resolved.address, &line_context).await?;
                if line_context.is_cancelled() {
                    return Err(cancellation(&line_context));
                }
                let Some(loaded) = loaded else { return Ok(None) };
                check_loaded(&definition, &loaded)?;
                let release = Release::new();
                let report = core.clone();
                let watch = CommittedWatch::new(loaded.value(), release.clone(), move |e| report.report(e), None);
                core.attach_observer(&loaded, Arc::new(watch.clone()), &release);
                Ok(Some(watch))
            })
        });
        async move {
            let Some(watch) = run?.await? else { return Ok(None) };
            if context.is_cancelled() {
                watch.cancel();
                return Err(cancellation(&context));
            }
            if let Some(signal) = context.abort_signal() {
                watch.observe_cancellation(&signal)?;
            }
            Ok(Some(watch))
        }
    }
}
