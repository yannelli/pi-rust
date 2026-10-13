//! Read-only document access: snapshots, historical snapshots, and the on-line read the Harness views use.

use std::future::Future;
use std::sync::{Arc, Mutex};

use pi_chord::delta::track;
use pi_chord::{BoxFuture, Context, Json};

use super::core::{LoadedDocument, LoadedMeta, OnLineDocument, Session, SessionCore};
use super::line::lock;
use crate::documents::{
    ConversationDocScope, DocDefinition, DocFamilyToken, DocScope, DocToken, DocValue, ResolvedAddress,
    RewindableDocScope, check_record_scope, check_record_version, materialize_document,
};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, EntryId};
use crate::types::{DocumentAddress, DocumentPoint, DocumentScope, Storage};

pub(crate) type Def = Arc<DocDefinition>;

pub(crate) fn check_loaded(definition: &DocDefinition, loaded: &LoadedDocument) -> DurableResult<()> {
    check_record_scope(definition, &loaded.record)?;
    check_record_version(definition, &loaded.record, loaded.stored_version())
}

impl SessionCore {
    /// The cached current incarnation, cold-loading and migrating it when necessary.
    pub(crate) async fn load_document(
        &self,
        definition: &DocDefinition,
        address_id: &str,
        address: &DocumentAddress,
        context: &Context,
    ) -> DurableResult<Option<Arc<LoadedDocument>>> {
        {
            let mut documents = lock(&self.documents);
            // A tracker serves only tokens of the version its value was materialized for; others reload.
            match documents.get(address_id) {
                Some(cached) if cached.value_version == definition.version => return Ok(Some(cached.clone())),
                Some(_) => {
                    documents.remove(address_id);
                }
                None => {}
            }
        }
        let Some(record) = self.storage.find_document(address, DocumentPoint::Current, context).await? else {
            return Ok(None);
        };
        let Some(stored) = self.storage.document(record.id, DocumentPoint::Current, context).await? else {
            let message = format!("Current document {} ({}) cannot be read", record.id, record.kind);
            return Err(DurableError::error(message));
        };
        let record = stored.record.clone();
        let meta = LoadedMeta { stored_version: stored.version, deltas_since_base: stored.deltas_since_base };
        let value = materialize_document(definition, stored)?;
        let loaded = Arc::new(LoadedDocument {
            address_id: address_id.to_owned(),
            record,
            value_version: definition.version,
            tracker: Arc::new(Mutex::new(track(value))),
            meta: Mutex::new(meta),
        });
        lock(&self.documents).insert(address_id.to_owned(), loaded.clone());
        Ok(Some(loaded))
    }

    /// Queue a cold load on the line.
    pub(crate) fn load_on_line(
        self: &Arc<Self>,
        definition: Def,
        resolved: ResolvedAddress,
        context: &Context,
    ) -> BoxFuture<DurableResult<Option<Arc<LoadedDocument>>>> {
        let core = self.clone();
        let context = context.clone();
        Box::pin(self.enqueue(async move {
            core.assert_healthy()?;
            core.load_document(&definition, &resolved.id, &resolved.address, &context).await
        }))
    }
}

impl Session {
    /// Snapshot of the singleton for `owner`: the current shareable immutable revision, or `None` when absent.
    pub fn snapshot<T: DocValue, S: DocScope>(
        &self,
        token: &DocToken<T, S>,
        owner: S::Owner,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<Json>>> + Send + use<T, S> {
        self.snapshot_erased(token.definition().clone(), token.resolve(owner), context)
    }

    /// Snapshot of the family member `key`; snapshots never take a creation seed.
    pub fn family_snapshot<T: DocValue, I: DocValue, S: DocScope>(
        &self,
        token: &DocFamilyToken<T, I, S>,
        owner: S::Owner,
        key: &str,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<Json>>> + Send + use<T, I, S> {
        self.snapshot_erased(token.definition().clone(), token.resolve(owner, key), context)
    }

    /// Erased snapshot; never creates or persists a migration.
    pub fn snapshot_erased(
        &self,
        definition: Def,
        resolved: ResolvedAddress,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<Json>>> + Send + use<> {
        let usable = self.0.assert_usable();
        let cached = lock(&self.0.documents)
            .get(&resolved.id)
            .filter(|cached| cached.value_version == definition.version)
            .cloned();
        let load = match (&usable, &cached) {
            (Ok(()), None) => Some(self.0.load_on_line(definition.clone(), resolved, context)),
            _ => None,
        };
        async move {
            usable?;
            let loaded = match (cached, load) {
                (Some(cached), _) => Some(cached),
                (None, Some(load)) => load.await?,
                (None, None) => None,
            };
            let Some(loaded) = loaded else { return Ok(None) };
            check_loaded(&definition, &loaded)?;
            Ok(Some(loaded.value()))
        }
    }

    /// The rewindable singleton as of entry `at`, read from the ancestor that owns the entry.
    pub fn snapshot_as_of<T: DocValue, S: RewindableDocScope>(
        &self,
        token: &DocToken<T, S>,
        conversation_id: ConversationId,
        at: EntryId,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<Json>>> + Send + use<T, S> {
        self.snapshot_as_of_erased(token.definition().clone(), token.resolve(conversation_id), at, context)
    }

    pub fn family_snapshot_as_of<T: DocValue, I: DocValue, S: RewindableDocScope>(
        &self,
        token: &DocFamilyToken<T, I, S>,
        conversation_id: ConversationId,
        key: &str,
        at: EntryId,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<Json>>> + Send + use<T, I, S> {
        let resolved = token.resolve(conversation_id, key);
        self.snapshot_as_of_erased(token.definition().clone(), resolved, at, context)
    }

    /// Erased `snapshot_as_of`: selects the incarnation alive at the entry's commit.
    pub fn snapshot_as_of_erased(
        &self,
        definition: Def,
        resolved: ResolvedAddress,
        at: EntryId,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<Json>>> + Send + use<> {
        let usable = self.0.assert_usable();
        let core = self.0.clone();
        let context = context.clone();
        let run = usable.and_then(|()| {
            let DocumentScope::Conversation { conversation_id } = resolved.address.scope else {
                return Err(DurableError::type_error("Session.snapshotAsOf() requires a conversation document"));
            };
            Ok(self.0.enqueue(async move {
                core.assert_healthy()?;
                let Some(entry) = core.storage.visible_entry(conversation_id, at, &context).await? else {
                    let message = format!("Entry {at} is not visible from conversation {conversation_id}");
                    return Err(DurableError::error(message));
                };
                let scope = DocumentScope::Conversation { conversation_id: entry.entry.conversation_id };
                let address = DocumentAddress { scope, ..resolved.address };
                let point = DocumentPoint::At(entry.commit_seq);
                let Some(record) = core.storage.find_document(&address, point, &context).await? else {
                    return Ok(None);
                };
                let Some(stored) = core.storage.document(record.id, point, &context).await? else {
                    let message = format!("Historical document {} ({}) cannot be read", record.id, record.kind);
                    return Err(DurableError::error(message));
                };
                materialize_document(&definition, stored).map(Some)
            }))
        });
        async move { run?.await }
    }

    /// Internal: a conversation document's current incarnation and value, for a job already on the line.
    pub fn conversation_document_on_line<T: DocValue, S: ConversationDocScope>(
        &self,
        token: &DocToken<T, S>,
        conversation_id: ConversationId,
        context: &Context,
    ) -> impl Future<Output = DurableResult<Option<OnLineDocument>>> + Send + use<T, S> {
        let core = self.0.clone();
        let definition = token.definition().clone();
        let resolved = token.resolve(conversation_id);
        let context = context.clone();
        async move {
            let loaded = core.load_document(&definition, &resolved.id, &resolved.address, &context).await?;
            let Some(loaded) = loaded else { return Ok(None) };
            check_loaded(&definition, &loaded)?;
            let (record, version, value) = (loaded.record.clone(), loaded.value_version, loaded.value());
            Ok(Some(OnLineDocument { record, version, value }))
        }
    }
}
