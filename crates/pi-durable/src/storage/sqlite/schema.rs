//! Table and index names of the SQLite storage (upstream `storage/sqlite/schema.ts`), with an optional prefix so the
//! storage can share a database. Never change an existing name: a rename adds a new one so historical migrations keep
//! rendering their original object names.

use crate::errors::{DurableError, DurableResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteTables {
    pub durable_schema: String,
    pub durable_metadata: String,
    pub record_ids: String,
    pub conversations: String,
    pub entries: String,
    pub tasks: String,
    pub submissions: String,
    pub documents: String,
    pub document_revisions: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteIndexes {
    pub conversations_by_owner_conversation: String,
    pub conversations_by_owner_task: String,
    pub entries_by_conversation: String,
    pub entry_heads_by_conversation: String,
    pub tasks_by_status: String,
    pub tasks_by_conversation: String,
    pub tasks_by_kind: String,
    pub tasks_by_abort_requested: String,
    pub tasks_by_background: String,
    pub submissions_by_request: String,
    pub submissions_by_conversation: String,
    pub submissions_by_status: String,
    pub documents_by_address: String,
    pub documents_by_scope: String,
    pub documents_by_scope_kind: String,
    pub document_revisions_by_kind: String,
}

/// The identifiers every migration and storage query uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteSchema {
    pub tables: SqliteTables,
    pub indexes: SqliteIndexes,
}

impl Default for SqliteSchema {
    fn default() -> Self {
        Self::with_prefix("")
    }
}

impl SqliteSchema {
    /// The schema with every table and index name prefixed; `None` for the unprefixed names.
    pub fn new(prefix: Option<&str>) -> DurableResult<Self> {
        if let Some(prefix) = prefix {
            validate_sqlite_schema_prefix(prefix)?;
        }
        Ok(Self::with_prefix(prefix.unwrap_or_default()))
    }

    fn with_prefix(prefix: &str) -> Self {
        let name = |name: &str| format!("{prefix}{name}");
        Self {
            tables: SqliteTables {
                durable_schema: name("durable_schema"),
                durable_metadata: name("durable_metadata"),
                record_ids: name("record_ids"),
                conversations: name("conversations"),
                entries: name("entries"),
                tasks: name("tasks"),
                submissions: name("submissions"),
                documents: name("documents"),
                document_revisions: name("document_revisions"),
            },
            indexes: SqliteIndexes {
                conversations_by_owner_conversation: name("conversations_by_owner_conversation"),
                conversations_by_owner_task: name("conversations_by_owner_task"),
                entries_by_conversation: name("entries_by_conversation"),
                entry_heads_by_conversation: name("entry_heads_by_conversation"),
                tasks_by_status: name("tasks_by_status"),
                tasks_by_conversation: name("tasks_by_conversation"),
                tasks_by_kind: name("tasks_by_kind"),
                tasks_by_abort_requested: name("tasks_by_abort_requested"),
                tasks_by_background: name("tasks_by_background"),
                submissions_by_request: name("submissions_by_request"),
                submissions_by_conversation: name("submissions_by_conversation"),
                submissions_by_status: name("submissions_by_status"),
                documents_by_address: name("documents_by_address"),
                documents_by_scope: name("documents_by_scope"),
                documents_by_scope_kind: name("documents_by_scope_kind"),
                document_revisions_by_kind: name("document_revisions_by_kind"),
            },
        }
    }
}

/// A prefix must be a SQLite identifier and must not use SQLite's reserved `sqlite_` prefix.
pub fn validate_sqlite_schema_prefix(prefix: &str) -> DurableResult<()> {
    let mut chars = prefix.chars();
    let identifier = chars.next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !identifier {
        return Err(DurableError::error(format!("Invalid SQLite schema prefix {}", serde_json::json!(prefix))));
    }
    if prefix.to_ascii_lowercase().starts_with("sqlite_") {
        return Err(DurableError::error(format!("SQLite schema prefix {} is reserved", serde_json::json!(prefix))));
    }
    Ok(())
}
