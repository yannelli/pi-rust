//! Document definitions (spec section 3.1), address resolution, and the stored-version checks typed access applies
//! before it uses a value. Typed tokens live in [`tokens`].

mod scopes;
mod tokens;

use std::fmt;
use std::sync::Arc;

use pi_chord::Json;
use pi_chord::delta::Op;

pub use scopes::*;
pub use tokens::*;

use crate::errors::{DurableError, DurableResult};
use crate::ids::DocumentId;
use crate::types::{
    DocumentAddress, DocumentCreate, DocumentFork, DocumentHistory, DocumentRecord, DocumentScope, StoredDocument,
};

/// Scope and, for conversation documents, history and fork policy of a definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DocSemantics {
    Session,
    Conversation { history: DocumentHistory, fork: DocumentFork },
    Task,
}

impl DocSemantics {
    /// `"session"`, `"conversation"`, or `"task"`.
    pub fn scope_kind(&self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Conversation { .. } => "conversation",
            Self::Task => "task",
        }
    }
}

/// Information passed to `checkpoint_when`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointInfo {
    /// Deltas already stored after the newest base, excluding this change.
    pub deltas_since_base: u32,
}

type InitialFn = Arc<dyn Fn(Option<&Json>) -> DurableResult<Json> + Send + Sync>;
type MigrateFn = Arc<dyn Fn(&Json, u32) -> DurableResult<Json> + Send + Sync>;
pub(crate) type CheckpointFn = Arc<dyn Fn(&Json, &[Op], CheckpointInfo) -> DurableResult<bool> + Send + Sync>;

/// Erased definition used by the Session after overload resolution (upstream `AnyDocDefinition`).
pub struct DocDefinition {
    pub kind: String,
    pub version: u32,
    pub semantics: DocSemantics,
    /// Whether this is a family whose members are addressed by key and created from a seed.
    pub family: bool,
    initial: InitialFn,
    migrate: Option<MigrateFn>,
    checkpoint_when: CheckpointFn,
}

impl fmt::Debug for DocDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocDefinition")
            .field("kind", &self.kind)
            .field("version", &self.version)
            .field("semantics", &self.semantics)
            .field("family", &self.family)
            .finish()
    }
}

impl DocDefinition {
    /// Build an erased definition from JSON callbacks. Typed code uses [`define_doc`] or [`define_doc_family`].
    ///
    /// `checkpoint_when` is required and has no default (spec section 3.5).
    pub fn new(
        kind: impl Into<String>,
        version: u32,
        semantics: DocSemantics,
        family: bool,
        initial: impl Fn(Option<&Json>) -> DurableResult<Json> + Send + Sync + 'static,
        checkpoint_when: impl Fn(&Json, &[Op], CheckpointInfo) -> DurableResult<bool> + Send + Sync + 'static,
    ) -> DurableResult<Self> {
        Self::from_shared(kind, version, semantics, family, Arc::new(initial), Arc::new(checkpoint_when))
    }

    pub(crate) fn from_shared(
        kind: impl Into<String>,
        version: u32,
        semantics: DocSemantics,
        family: bool,
        initial: InitialFn,
        checkpoint_when: CheckpointFn,
    ) -> DurableResult<Self> {
        let kind = kind.into();
        if version < 1 {
            return Err(DurableError::type_error(format!("Document {kind} version must be a positive integer")));
        }
        Ok(Self { kind, version, semantics, family, initial, migrate: None, checkpoint_when })
    }

    pub fn with_migrate(mut self, migrate: impl Fn(&Json, u32) -> DurableResult<Json> + Send + Sync + 'static) -> Self {
        self.migrate = Some(Arc::new(migrate));
        self
    }

    /// The first value of a new incarnation; `seed` is the family creation seed.
    pub fn initial(&self, seed: Option<&Json>) -> DurableResult<Json> {
        (self.initial)(seed)
    }

    pub fn has_migrate(&self) -> bool {
        self.migrate.is_some()
    }

    /// Migrate a detached older value to this version. Fails when the definition has no migration.
    pub fn migrate(&self, value: &Json, from_version: u32) -> DurableResult<Json> {
        match &self.migrate {
            Some(migrate) => migrate(value, from_version),
            None => Err(DurableError::error(format!("Document {} has no migration", self.kind))),
        }
    }

    /// Whether this change stores a complete base.
    pub fn checkpoint_when(&self, value: &Json, ops: &[Op], info: CheckpointInfo) -> DurableResult<bool> {
        (self.checkpoint_when)(value, ops, info)
    }

    /// Resolve the logical address of a singleton (`key` absent) or family member in `scope`.
    pub fn resolve(&self, scope: DocumentScope, key: Option<String>) -> ResolvedAddress {
        let address = DocumentAddress { kind: self.kind.clone(), scope, key };
        ResolvedAddress { id: address_id(&address), address }
    }
}

/// Logical address plus its string identity for maps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAddress {
    pub address: DocumentAddress,
    pub id: String,
}

/// Stable string identity of one logical address: `[kind, scope kind, owner, key]` as JSON.
pub fn address_id(address: &DocumentAddress) -> String {
    let owner = address.scope.owner_id().map_or(Json::Null, Json::from);
    let key = address.key.as_deref().map_or(Json::Null, Json::from);
    Json::array(vec![address.kind.as_str().into(), address.scope.kind().into(), owner, key]).to_string()
}

/// Build the storage create record for a new incarnation at an address.
pub fn document_create(definition: &DocDefinition, address: &DocumentAddress, id: DocumentId) -> DocumentCreate {
    let (history, fork) = match (address.scope, definition.semantics) {
        (DocumentScope::Conversation { .. }, DocSemantics::Conversation { history, fork }) => {
            (Some(history), Some(fork))
        }
        _ => (None, None),
    };
    DocumentCreate { id, kind: address.kind.clone(), key: address.key.clone(), scope: address.scope, history, fork }
}

/// Record fields typed access checks; implemented by `DocumentCreate` and `DocumentRecord`.
pub trait DocumentIdentity {
    fn document_id(&self) -> DocumentId;
    fn document_kind(&self) -> &str;
    fn document_scope(&self) -> DocumentScope;
    fn document_history(&self) -> Option<DocumentHistory>;
    fn document_fork(&self) -> Option<DocumentFork>;
}

macro_rules! identity {
    ($type:ty) => {
        impl DocumentIdentity for $type {
            fn document_id(&self) -> DocumentId {
                self.id
            }
            fn document_kind(&self) -> &str {
                &self.kind
            }
            fn document_scope(&self) -> DocumentScope {
                self.scope
            }
            fn document_history(&self) -> Option<DocumentHistory> {
                self.history
            }
            fn document_fork(&self) -> Option<DocumentFork> {
                self.fork
            }
        }
    };
}
identity!(DocumentCreate);
identity!(DocumentRecord);

/// Reject typed access whose token disagrees with the persisted scope, history, or fork semantics.
pub fn check_record_scope(definition: &DocDefinition, record: &impl DocumentIdentity) -> DurableResult<()> {
    let scope = record.document_scope();
    let matches = match definition.semantics {
        DocSemantics::Session => scope.kind() == "session",
        DocSemantics::Task => scope.kind() == "task",
        DocSemantics::Conversation { history, fork } => {
            scope.kind() == "conversation"
                && record.document_history() == Some(history)
                && record.document_fork() == Some(fork)
        }
    };
    if matches {
        return Ok(());
    }
    Err(DurableError::type_error(format!(
        "Document {} ({}) does not match the supplied definition semantics",
        record.document_id(),
        record.document_kind()
    )))
}

/// Reject typed access to a stored version the supplied definition cannot use.
pub fn check_record_version(
    definition: &DocDefinition,
    record: &impl DocumentIdentity,
    version: u32,
) -> DurableResult<()> {
    let (id, kind) = (record.document_id(), record.document_kind());
    if version > definition.version {
        return Err(DurableError::error(format!(
            "Document {id} ({kind}) has newer version {version} than {}",
            definition.version
        )));
    }
    if version < definition.version && !definition.has_migrate() {
        return Err(DurableError::error(format!("Document {id} ({kind}) requires migration from version {version}")));
    }
    Ok(())
}

/// Validate and materialize a detached stored value for typed access.
pub fn materialize_document(definition: &DocDefinition, stored: StoredDocument) -> DurableResult<Json> {
    materialize_document_value(definition, &stored.record, stored.version, stored.value)
}

/// Validate and materialize one detached value before its first persisted incarnation.
pub fn materialize_document_value(
    definition: &DocDefinition,
    record: &impl DocumentIdentity,
    version: u32,
    value: Json,
) -> DurableResult<Json> {
    check_record_scope(definition, record)?;
    check_record_version(definition, record, version)?;
    if version == definition.version {
        return Ok(value);
    }
    definition.migrate(&value, version)
}
