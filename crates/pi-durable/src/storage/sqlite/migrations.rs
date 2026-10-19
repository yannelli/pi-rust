//! Ordered, immutable schema history of the SQLite storage, byte-for-byte the upstream SQL for a given schema.

use std::sync::LazyLock;

use super::database::{SqliteDatabase, SqliteValue, transaction};
use super::schema::SqliteSchema;
use crate::errors::{DurableError, DurableResult};

/// One schema version and the statements that create it.
#[derive(Clone, Debug)]
pub struct SqliteMigration {
    pub version: i64,
    pub statements: Vec<String>,
}

/// Immutable, ordered schema history rendered for `schema`. New migrations are appended after the shipped ones.
pub fn create_sqlite_migrations(schema: &SqliteSchema) -> Vec<SqliteMigration> {
    let (t, i) = (&schema.tables, &schema.indexes);
    // next_id is TEXT because node:sqlite rejects INTEGER results outside JavaScript's safe integer range.
    let initial_schema = vec![
        format!(
            r#"CREATE TABLE {} (
		singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
		next_id TEXT NOT NULL,
		next_seq INTEGER NOT NULL
	) STRICT"#,
            t.durable_metadata
        ),
        format!(r#"INSERT INTO {} (singleton, next_id, next_seq) VALUES (1, '2', 1)"#, t.durable_metadata),
        format!(
            r#"CREATE TABLE {} (
		id INTEGER PRIMARY KEY,
		record_type TEXT NOT NULL CHECK (record_type IN ('conversation', 'entry', 'task', 'submission', 'document'))
	) STRICT"#,
            t.record_ids
        ),
        format!(
            r#"CREATE TABLE {} (
		id INTEGER PRIMARY KEY,
		owner_conversation_id INTEGER,
		owner_task_id INTEGER,
		record TEXT NOT NULL CHECK (json_valid(record))
	) STRICT"#,
            t.conversations
        ),
        format!(
            r#"CREATE INDEX {} ON {} (owner_conversation_id, id)"#,
            i.conversations_by_owner_conversation, t.conversations
        ),
        format!(r#"CREATE INDEX {} ON {} (owner_task_id, id)"#, i.conversations_by_owner_task, t.conversations),
        format!(
            r#"CREATE TABLE {} (
		id INTEGER PRIMARY KEY,
		conversation_id INTEGER NOT NULL,
		head INTEGER,
		commit_seq INTEGER NOT NULL,
		record TEXT NOT NULL CHECK (json_valid(record))
	) STRICT"#,
            t.entries
        ),
        format!(r#"CREATE INDEX {} ON {} (conversation_id, id DESC)"#, i.entries_by_conversation, t.entries),
        format!(
            r#"CREATE INDEX {} ON {} (conversation_id, id DESC) WHERE head IS NOT NULL"#,
            i.entry_heads_by_conversation, t.entries
        ),
        format!(
            r#"CREATE TABLE {} (
		id INTEGER PRIMARY KEY,
		conversation_id INTEGER NOT NULL,
		kind TEXT NOT NULL,
		status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'waiting', 'completing', 'terminal')),
		abort_requested INTEGER NOT NULL CHECK (abort_requested IN (0, 1)),
		background INTEGER NOT NULL CHECK (background IN (0, 1)),
		record TEXT NOT NULL CHECK (json_valid(record))
	) STRICT"#,
            t.tasks
        ),
        format!(r#"CREATE INDEX {} ON {} (status, id)"#, i.tasks_by_status, t.tasks),
        format!(r#"CREATE INDEX {} ON {} (conversation_id, id)"#, i.tasks_by_conversation, t.tasks),
        format!(r#"CREATE INDEX {} ON {} (kind, id)"#, i.tasks_by_kind, t.tasks),
        format!(r#"CREATE INDEX {} ON {} (abort_requested, id)"#, i.tasks_by_abort_requested, t.tasks),
        format!(r#"CREATE INDEX {} ON {} (background, id)"#, i.tasks_by_background, t.tasks),
        format!(
            r#"CREATE TABLE {} (
		id INTEGER PRIMARY KEY,
		conversation_id INTEGER NOT NULL,
		request_id TEXT,
		status TEXT NOT NULL CHECK (status IN ('queued', 'placed', 'done', 'unanswered')),
		record TEXT NOT NULL CHECK (json_valid(record))
	) STRICT"#,
            t.submissions
        ),
        format!(r#"CREATE INDEX {} ON {} (conversation_id, request_id)"#, i.submissions_by_request, t.submissions),
        format!(r#"CREATE INDEX {} ON {} (conversation_id, id)"#, i.submissions_by_conversation, t.submissions),
        format!(r#"CREATE INDEX {} ON {} (status, id)"#, i.submissions_by_status, t.submissions),
        format!(
            r#"CREATE TABLE {} (
		id INTEGER PRIMARY KEY,
		kind TEXT NOT NULL,
		family INTEGER NOT NULL CHECK (family IN (0, 1)),
		key_value TEXT NOT NULL,
		scope_kind TEXT NOT NULL CHECK (scope_kind IN ('session', 'conversation', 'task')),
		owner_id INTEGER NOT NULL,
		created_at INTEGER NOT NULL,
		retired_at INTEGER,
		record TEXT NOT NULL CHECK (json_valid(record))
	) STRICT"#,
            t.documents
        ),
        format!(
            r#"CREATE INDEX {}
		ON {} (kind, scope_kind, owner_id, family, key_value, created_at DESC, retired_at)"#,
            i.documents_by_address, t.documents
        ),
        format!(r#"CREATE INDEX {} ON {} (scope_kind, owner_id, id)"#, i.documents_by_scope, t.documents),
        format!(r#"CREATE INDEX {} ON {} (scope_kind, owner_id, kind, id)"#, i.documents_by_scope_kind, t.documents),
        format!(
            r#"CREATE TABLE {} (
		document_id INTEGER NOT NULL,
		seq INTEGER NOT NULL,
		kind TEXT NOT NULL CHECK (kind IN ('base', 'delta')),
		version INTEGER NOT NULL,
		content TEXT NOT NULL CHECK (json_valid(content)),
		PRIMARY KEY (document_id, seq)
	) STRICT"#,
            t.document_revisions
        ),
        format!(
            r#"CREATE INDEX {} ON {} (document_id, kind, seq DESC)"#,
            i.document_revisions_by_kind, t.document_revisions
        ),
    ];
    // Version 2 adds the `sleeping` task status. SQLite cannot change a CHECK constraint in place, so the table is rebuilt.
    let rebuilt_tasks = format!("{}_v2", t.tasks);
    let sleeping_status = vec![
        format!(
            r#"CREATE TABLE {rebuilt_tasks} (
			id INTEGER PRIMARY KEY,
			conversation_id INTEGER NOT NULL,
			kind TEXT NOT NULL,
			status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'waiting', 'sleeping', 'completing', 'terminal')),
			abort_requested INTEGER NOT NULL CHECK (abort_requested IN (0, 1)),
			background INTEGER NOT NULL CHECK (background IN (0, 1)),
			record TEXT NOT NULL CHECK (json_valid(record))
		) STRICT"#
        ),
        format!(
            r#"INSERT INTO {rebuilt_tasks} (id, conversation_id, kind, status, abort_requested, background, record)
			SELECT id, conversation_id, kind, status, abort_requested, background, record FROM {}"#,
            t.tasks
        ),
        format!(r#"DROP TABLE {}"#, t.tasks),
        format!(r#"ALTER TABLE {rebuilt_tasks} RENAME TO {}"#, t.tasks),
        format!(r#"CREATE INDEX {} ON {} (status, id)"#, i.tasks_by_status, t.tasks),
        format!(r#"CREATE INDEX {} ON {} (conversation_id, id)"#, i.tasks_by_conversation, t.tasks),
        format!(r#"CREATE INDEX {} ON {} (kind, id)"#, i.tasks_by_kind, t.tasks),
        format!(r#"CREATE INDEX {} ON {} (abort_requested, id)"#, i.tasks_by_abort_requested, t.tasks),
        format!(r#"CREATE INDEX {} ON {} (background, id)"#, i.tasks_by_background, t.tasks),
    ];
    vec![
        SqliteMigration { version: 1, statements: initial_schema },
        SqliteMigration { version: 2, statements: sleeping_status },
    ]
}

/// The schema history for the unprefixed names.
pub static SQLITE_MIGRATIONS: LazyLock<Vec<SqliteMigration>> =
    LazyLock::new(|| create_sqlite_migrations(&SqliteSchema::default()));

pub const CURRENT_SQLITE_SCHEMA_VERSION: i64 = 2;

/// Apply all pending schema migrations for the unprefixed names atomically.
pub async fn apply_sqlite_migrations(
    database: &dyn SqliteDatabase,
    migrations: &[SqliteMigration],
) -> DurableResult<()> {
    apply_sqlite_migrations_for_schema(database, &SqliteSchema::default(), migrations).await
}

/// Apply migrations rendered for a particular schema atomically.
pub async fn apply_sqlite_migrations_for_schema(
    database: &dyn SqliteDatabase,
    schema: &SqliteSchema,
    migrations: &[SqliteMigration],
) -> DurableResult<()> {
    for (index, migration) in migrations.iter().enumerate() {
        if migration.version != index as i64 + 1 {
            return Err(DurableError::storage("Durable SQLite migrations must have contiguous versions starting at 1"));
        }
    }
    let table = schema.tables.durable_schema.clone();
    transaction(database, move |transaction| {
        Box::pin(async move {
            transaction
                .exec(&format!(
                    "CREATE TABLE IF NOT EXISTS {table} (
\t\t\tsingleton INTEGER PRIMARY KEY CHECK (singleton = 1),
\t\t\tversion INTEGER NOT NULL CHECK (version >= 0)
\t\t) STRICT"
                ))
                .await?;
            let insert = format!("INSERT OR IGNORE INTO {table} (singleton, version) VALUES (1, 0)");
            transaction.run(&insert, &[]).await?;
            let row = transaction.get(&format!("SELECT version FROM {table} WHERE singleton = 1"), &[]).await?;
            let Some(row) = row else {
                return Err(DurableError::storage("Durable SQLite schema metadata is missing"));
            };
            let stored = row.integer("version")?;
            let current = migrations.last().map_or(0, |migration| migration.version);
            if stored > current {
                return Err(DurableError::storage(format!(
                    "Durable SQLite schema version {stored} is newer than supported version {current}"
                )));
            }
            let update = format!("UPDATE {table} SET version = ? WHERE singleton = 1");
            for migration in migrations.iter().filter(|migration| migration.version > stored) {
                for statement in &migration.statements {
                    transaction.exec(statement).await?;
                }
                transaction.run(&update, &[SqliteValue::Integer(migration.version)]).await?;
            }
            Ok(())
        })
    })
    .await
}
