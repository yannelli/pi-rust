//! Document acquisition and retirement inside a transaction (spec section 3.3).

use std::future::Future;
use std::sync::{Arc, Mutex};

use pi_chord::Json;
use pi_chord::delta::track;

use super::draft::{Draft, DraftCore};
use super::line::{lock, ready_shared};
use super::tx::{DocumentEntry, DocumentTarget, Tx, TxFuture, settled};
use crate::documents::{
    DocDefinition, DocFamilyToken, DocScope, DocToken, DocValue, ResolvedAddress, check_record_scope,
    check_record_version, document_create, encode, materialize_document_value,
};
use crate::errors::{DurableError, DurableResult};
use crate::ids::DocumentId;
use crate::types::{DocumentCopySource, DocumentCreate, DocumentPoint, DocumentScope, Storage};

async fn typed<T: DocValue>(draft: TxFuture<DraftCore>) -> DurableResult<Draft<T>> {
    Ok(Draft::new(draft.await?))
}

impl Tx {
    /// Get or create the singleton for `owner` and return its draft; repeated access returns the same draft.
    pub fn doc<T: DocValue, S: DocScope>(
        &self,
        token: &DocToken<T, S>,
        owner: S::Owner,
    ) -> impl Future<Output = DurableResult<Draft<T>>> + Send + 'static {
        typed(self.doc_erased(token.definition().clone(), token.resolve(owner), None))
    }

    /// Get or create the family member `key`; the first acquisition of an absent member selects its seed.
    pub fn family_doc<T: DocValue, I: DocValue, S: DocScope>(
        &self,
        token: &DocFamilyToken<T, I, S>,
        owner: S::Owner,
        key: impl Into<String>,
        seed: &I,
    ) -> impl Future<Output = DurableResult<Draft<T>>> + Send + 'static {
        let resolved = token.resolve(owner, key);
        let draft = match encode(seed) {
            Ok(seed) => self.doc_erased(token.definition().clone(), resolved, Some(seed)),
            Err(error) => ready_shared(Err(error)),
        };
        typed(draft)
    }

    /// Retire the singleton for `owner` without creating it. An acquired draft persists its final content first.
    pub fn retire_doc<T: DocValue, S: DocScope>(&self, token: &DocToken<T, S>, owner: S::Owner) -> TxFuture<()> {
        self.retire_erased(token.definition().clone(), token.resolve(owner))
    }

    pub fn retire_family_doc<T: DocValue, I: DocValue, S: DocScope>(
        &self,
        token: &DocFamilyToken<T, I, S>,
        owner: S::Owner,
        key: impl Into<String>,
    ) -> TxFuture<()> {
        self.retire_erased(token.definition().clone(), token.resolve(owner, key))
    }

    /// Erased `doc`: `seed` is the family creation seed.
    pub fn doc_erased(
        &self,
        definition: Arc<DocDefinition>,
        resolved: ResolvedAddress,
        seed: Option<Json>,
    ) -> TxFuture<DraftCore> {
        let mut state = match self.0.open_state() {
            Ok(state) => state,
            Err(error) => return ready_shared(Err(error)),
        };
        if let DocumentScope::Task { task_id } = resolved.address.scope
            && state.tasks.get(&task_id).and_then(|task| task.write.as_ref()).is_some_and(|w| w.1.is_terminal())
        {
            return ready_shared(Err(DurableError::error(format!("Task {task_id} is terminal"))));
        }
        let latest = state.latest_document_by_address.get(&resolved.id).copied();
        let mut skip_load = false;
        if let Some(index) = latest {
            let entry = &mut state.documents[index];
            if !entry.retire_on_commit {
                if let Some(draft) = &entry.draft {
                    return draft.clone();
                }
                if let Some(DocumentTarget::ForkCopy { record, source }) = entry.target.clone() {
                    let draft = self.track(self.clone().acquire_fork_copy(index, definition, record, source));
                    entry.draft = Some(draft.clone());
                    return draft;
                }
            }
            skip_load = entry.retire_on_commit;
        }
        let mut entry = DocumentEntry::new(resolved.id, resolved.address);
        entry.definition = Some(definition);
        let index = state.push_document(entry);
        // Capture retirement before awaiting so a pending old acquisition and its replacement stay distinct.
        let draft = self.track(self.clone().acquire(index, seed, skip_load));
        state.documents[index].draft = Some(draft.clone());
        draft
    }

    /// Erased `retire_doc`.
    pub fn retire_erased(&self, definition: Arc<DocDefinition>, resolved: ResolvedAddress) -> TxFuture<()> {
        let mut state = match self.0.open_state() {
            Ok(state) => state,
            Err(error) => return ready_shared(Err(error)),
        };
        if let Some(index) = state.latest_document_by_address.get(&resolved.id).copied() {
            let entry = &mut state.documents[index];
            if entry.retire_on_commit {
                return ready_shared(Ok(()));
            }
            if let Some(DocumentTarget::ForkCopy { record, .. }) = &entry.target {
                let checked = check_record_scope(&definition, record);
                if checked.is_ok() {
                    entry.retire_on_commit = true;
                }
                return ready_shared(checked);
            }
            if let Some(draft) = entry.draft.clone() {
                entry.retire_on_commit = true;
                return self.track(async move { draft.await.map(|_| ()) });
            }
        }
        let mut entry = DocumentEntry::new(resolved.id, resolved.address);
        entry.definition = Some(definition);
        entry.retire_on_commit = true;
        let index = state.push_document(entry);
        self.track(self.clone().find_retirement(index))
    }

    fn open_change(
        &self,
        index: usize,
        target: DocumentTarget,
        tracker: &Arc<Mutex<pi_chord::delta::Tracker>>,
    ) -> DurableResult<DraftCore> {
        let mut state = self.0.open_state()?;
        let cell = Arc::new(Mutex::new(lock(tracker).begin_change()));
        let entry = &mut state.documents[index];
        entry.target = Some(target);
        entry.change = Some(cell.clone());
        Ok(DraftCore { cell, tx: self.0.clone() })
    }

    async fn acquire(self, index: usize, seed: Option<Json>, skip_load: bool) -> DurableResult<DraftCore> {
        let tx = &self.0;
        let (definition, address_id, address) = {
            let state = lock(&tx.state);
            let entry = &state.documents[index];
            (entry.definition.clone().ok_or_else(settled)?, entry.address_id.clone(), entry.address.clone())
        };
        let loaded = if skip_load {
            None
        } else {
            tx.core.load_document(&definition, &address_id, &address, &tx.context).await?
        };
        tx.assert_open()?;
        if let Some(loaded) = loaded {
            check_record_scope(&definition, &loaded.record)?;
            check_record_version(&definition, &loaded.record, loaded.stored_version())?;
            let tracker = loaded.tracker.clone();
            return self.open_change(index, DocumentTarget::Loaded(loaded), &tracker);
        }
        match address.scope {
            DocumentScope::Conversation { conversation_id } => tx.require_conversation(conversation_id).await?,
            DocumentScope::Task { task_id } => match tx.current_task(task_id).await? {
                None => return Err(DurableError::error(format!("Task {task_id} does not exist"))),
                Some(task) if task.is_terminal() => {
                    return Err(DurableError::error(format!("Task {task_id} is terminal")));
                }
                Some(_) => {}
            },
            DocumentScope::Session => {}
        }
        tx.assert_open()?;
        let value = definition.initial(seed.as_ref())?;
        let id = DocumentId(tx.mint().await?);
        tx.assert_open()?;
        let record = document_create(&definition, &address, id);
        let tracker = Arc::new(Mutex::new(track(value)));
        let target = DocumentTarget::Created { record, version: definition.version, tracker: tracker.clone() };
        self.open_change(index, target, &tracker)
    }

    async fn acquire_fork_copy(
        self,
        index: usize,
        definition: Arc<DocDefinition>,
        record: DocumentCreate,
        source: DocumentCopySource,
    ) -> DurableResult<DraftCore> {
        let tx = &self.0;
        let stored = tx.core.storage.document(source.id, source.at, &tx.context).await?;
        tx.assert_open()?;
        let Some(stored) = stored else {
            return Err(DurableError::error(format!("Fork source document {} cannot be read", source.id)));
        };
        let source_record = &stored.record;
        if source_record.scope.kind() != "conversation"
            || source_record.kind != record.kind
            || source_record.key != record.key
            || source_record.history != record.history
            || source_record.fork != record.fork
        {
            return Err(DurableError::error(format!(
                "Fork source document {} does not match the copied record",
                source.id
            )));
        }
        let value = materialize_document_value(&definition, &record, stored.version, stored.value)?;
        let tracker = Arc::new(Mutex::new(track(value)));
        lock(&tx.state).documents[index].definition = Some(definition.clone());
        let target = DocumentTarget::Created { record, version: definition.version, tracker: tracker.clone() };
        self.open_change(index, target, &tracker)
    }

    async fn find_retirement(self, index: usize) -> DurableResult<()> {
        let tx = &self.0;
        let (definition, address_id, address) = {
            let state = lock(&tx.state);
            let entry = &state.documents[index];
            (entry.definition.clone().ok_or_else(settled)?, entry.address_id.clone(), entry.address.clone())
        };
        let cached = lock(&tx.core.documents).get(&address_id).map(|loaded| loaded.record.clone());
        let record = match cached {
            Some(record) => Some(record),
            None => tx.core.storage.find_document(&address, DocumentPoint::Current, &tx.context).await?,
        };
        tx.assert_open()?;
        let Some(record) = record else { return Ok(()) };
        check_record_scope(&definition, &record)?;
        tx.open_state()?.documents[index].target = Some(DocumentTarget::RetireOnly(record));
        Ok(())
    }
}
