//! Write and publication plans of staged documents, and submission changes, decided before Storage admission.

use std::sync::{Arc, Mutex};

use pi_chord::delta::{Prepared, Tracker};

use super::core::LoadedDocument;
use super::tx::{DocumentEntry, DocumentTarget, SubmissionChange};
use crate::documents::DocDefinition;
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, DocumentId};
use crate::types::*;

#[derive(Clone)]
pub(crate) enum PlanRecord {
    Committed(DocumentRecord),
    New(DocumentCreate),
}

impl PlanRecord {
    pub fn id(&self) -> DocumentId {
        match self {
            Self::Committed(record) => record.id,
            Self::New(record) => record.id,
        }
    }

    pub fn scope(&self) -> DocumentScope {
        match self {
            Self::Committed(record) => record.scope,
            Self::New(record) => record.scope,
        }
    }

    pub fn fork(&self) -> Option<DocumentFork> {
        match self {
            Self::Committed(record) => record.fork,
            Self::New(record) => record.fork,
        }
    }
}

pub(crate) struct PlanChange {
    pub tracker: Arc<Mutex<Tracker>>,
    pub prepared: Prepared,
    pub version: u32,
    /// The cached incarnation this change updates; absent when it creates one.
    pub loaded: Option<Arc<LoadedDocument>>,
    pub definition: Option<Arc<DocDefinition>>,
}

/// What one staged incarnation writes and publishes, decided before Storage admission.
pub(crate) struct DocumentPlan {
    pub address_id: String,
    pub record: PlanRecord,
    pub retire: bool,
    pub content: Option<StorageWrite>,
    pub change: Option<PlanChange>,
    pub conversation_id: Option<ConversationId>,
    /// Set when this commit also stores held changes of the incarnation (`take_held()`): the operations its stored
    /// change publishes, held first, and whether it has a change of its own to show the Session's observers.
    pub held: Option<(Vec<pi_chord::delta::Op>, bool)>,
}

impl DocumentPlan {
    /// Every creation, copy, and retirement, and a loaded incarnation that writes content.
    pub fn publishes(&self) -> bool {
        self.retire || self.change.as_ref().is_none_or(|change| change.loaded.is_none()) || self.content.is_some()
    }

    /// A retirement found by a terminal-task scan.
    pub fn retirement(address_id: String, record: DocumentRecord) -> Self {
        Self {
            address_id,
            record: PlanRecord::Committed(record),
            retire: true,
            content: None,
            change: None,
            conversation_id: None,
            held: None,
        }
    }
}

/// Plan of one staged document: its record, content write, and prepared change. Retirement is decided later.
pub(crate) fn plan_document(document: &DocumentEntry) -> Option<DocumentPlan> {
    let target = document.target.clone()?;
    let retire = document.retire_on_commit;
    let address_id = document.address_id.clone();
    let (record, content, change) = match target {
        DocumentTarget::Created { record, version, tracker } => {
            let prepared = document.prepared.clone()?;
            let content = DocumentContent::Base { version, value: prepared.value.clone() };
            let write = StorageWrite::DocumentCreate { record: record.clone(), content };
            let change = PlanChange { tracker, prepared, version, loaded: None, definition: None };
            (PlanRecord::New(record), Some(write), Some(change))
        }
        DocumentTarget::ForkCopy { record, source } => {
            let write = StorageWrite::DocumentCopy { record: record.clone(), source };
            (PlanRecord::New(record), Some(write), None)
        }
        DocumentTarget::RetireOnly(record) => (PlanRecord::Committed(record), None, None),
        DocumentTarget::Loaded(loaded) => {
            let definition = document.definition.clone()?;
            let prepared = document.prepared.clone()?;
            let version = definition.version;
            let id = loaded.record.id;
            // A version change stores a base even without operations; otherwise only a change stores a delta.
            let content = if loaded.stored_version() < version {
                let content = DocumentContent::Base { version, value: prepared.value.clone() };
                Some(StorageWrite::DocumentChange { id, content })
            } else if !prepared.ops.is_empty() {
                let content = DocumentContent::Delta { version, ops: prepared.ops.clone() };
                Some(StorageWrite::DocumentChange { id, content })
            } else {
                None
            };
            let record = PlanRecord::Committed(loaded.record.clone());
            let tracker = loaded.tracker.clone();
            let change = PlanChange { tracker, prepared, version, loaded: Some(loaded), definition: Some(definition) };
            (record, content, Some(change))
        }
    };
    Some(DocumentPlan { address_id, record, retire, content, change, conversation_id: None, held: None })
}

/// Complete record after one change; `None` when the record stays unchanged. Only a placed input is answered.
pub(crate) fn apply_submission_change(
    current: &SubmissionRecord,
    change: SubmissionChange,
) -> DurableResult<Option<SubmissionRecord>> {
    if current.status.is_settled() {
        return Ok(None);
    }
    let mut next = current.clone();
    match change {
        SubmissionChange::Place(entry) => {
            if current.status != SubmissionStatus::Queued {
                return Err(DurableError::error(format!("Submission {} is not queued", current.id)));
            }
            next.status = match current.kind {
                SubmissionType::Input => SubmissionStatus::Placed,
                SubmissionType::Write => SubmissionStatus::Done,
            };
            next.entry = Some(entry);
        }
        SubmissionChange::Settle(SubmissionSettlement::Done { answer }) => {
            if current.status != SubmissionStatus::Placed {
                return Err(DurableError::error(format!("Submission {} is not a placed input", current.id)));
            }
            next.status = SubmissionStatus::Done;
            next.answer = Some(answer);
        }
        SubmissionChange::Settle(SubmissionSettlement::Unanswered { reason, detail }) => {
            next.status = SubmissionStatus::Unanswered;
            next.reason = Some(reason);
            next.detail = detail;
        }
    }
    Ok(Some(next))
}
