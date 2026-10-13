//! Select the conversation documents a fork copies (spec section 3.7).

use std::collections::HashSet;

use pi_chord::Context;

use crate::documents::address_id;
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, DocumentId, EntryId};
use crate::types::{
    Cursor, DocumentCopySource, DocumentCreate, DocumentFork, DocumentPoint, DocumentQuery, DocumentScope, Storage,
};

const SCAN_PAGE_SIZE: usize = 256;

/// One definition-free document copy to create with a forked conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct ForkDocumentCopy {
    pub record: DocumentCreate,
    pub source: DocumentCopySource,
}

/// Select every persisted conversation document copied by one fork: `asOf` documents of the conversation owning
/// `at` at its commit, then `current` documents of the parent now.
pub async fn prepare_fork_document_copies(
    storage: &dyn Storage,
    parent_conversation_id: ConversationId,
    at: EntryId,
    child_conversation_id: ConversationId,
    context: &Context,
) -> DurableResult<Vec<ForkDocumentCopy>> {
    let Some(entry) = storage.visible_entry(parent_conversation_id, at, context).await? else {
        return Err(DurableError::error(format!(
            "Entry {at} is not visible from conversation {parent_conversation_id}"
        )));
    };
    let mut copies = Vec::new();
    let mut copied = HashSet::new();
    let selections = [
        (entry.entry.conversation_id, DocumentPoint::At(entry.commit_seq), DocumentFork::AsOf),
        (parent_conversation_id, DocumentPoint::Current, DocumentFork::Current),
    ];
    for (conversation_id, point, policy) in selections {
        let scope = DocumentScope::Conversation { conversation_id };
        let mut cursor: Option<Cursor> = None;
        loop {
            let query = DocumentQuery { scope, at: point, kind: None };
            let page = storage.scan_documents(&query, SCAN_PAGE_SIZE, cursor.as_ref(), context).await?;
            for source in page.items {
                if source.scope.kind() != "conversation" || source.fork != Some(policy) {
                    continue;
                }
                let id = DocumentId(storage.mint_id().await?);
                let record = DocumentCreate {
                    id,
                    kind: source.kind.clone(),
                    key: source.key.clone(),
                    scope: DocumentScope::Conversation { conversation_id: child_conversation_id },
                    history: source.history,
                    fork: source.fork,
                };
                if !copied.insert(address_id(&record.address())) {
                    let member = match &record.key {
                        Some(key) => format!("{}/{key}", record.kind),
                        None => record.kind.clone(),
                    };
                    return Err(DurableError::error(format!("Fork selects multiple source documents for {member}")));
                }
                copies.push(ForkDocumentCopy { record, source: DocumentCopySource { id: source.id, at: point } });
            }
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
    }
    Ok(copies)
}
