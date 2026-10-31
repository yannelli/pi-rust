//! Port of upstream `test/sqlite-facade.test.ts`. Rust futures start when polled, so "call order" here is the
//! order in which spawned tasks first run; on the current-thread runtime that is their spawn order.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::{BACKGROUND_CONTEXT, Json};
use pi_durable::storage::sqlite::{
    BundledSqliteDatabase, SqliteDatabase, SqliteExecutor, SqliteRow, SqliteStorage, SqliteStorageOptions, SqliteValue,
    TransactionCallback,
};
use pi_durable::*;
use tokio::sync::{Notify, oneshot};
use tokio::task::yield_now;

fn ctx() -> &'static pi_chord::Context {
    &BACKGROUND_CONTEXT
}

async fn memory_database() -> Arc<BundledSqliteDatabase> {
    Arc::new(BundledSqliteDatabase::open(":memory:", SqliteStorageOptions::default()).await.unwrap())
}

fn values(rows: &[SqliteRow]) -> Vec<i64> {
    rows.iter().map(|row| row.integer("value").unwrap()).collect()
}

fn entry(id: u64, kind: &str) -> StorageWrite {
    StorageWrite::Entry {
        value: EntryRecord {
            kind: kind.into(),
            model: None,
            data: None,
            edits: None,
            id: EntryId(id),
            conversation_id: ROOT_CONVERSATION_ID,
            head: None,
            by_task_id: None,
        },
    }
}

fn root() -> StorageWrite {
    StorageWrite::Conversation { value: ConversationRecord { id: ROOT_CONVERSATION_ID, parent: None, owner: None } }
}

/// Rusqlite does not expose how often a statement is prepared; this keeps the upstream workload and its results.
/// The adapter's statement cache holds 512 statements, more than the storage ever issues.
#[tokio::test]
async fn prepares_storage_statements_once_and_reuses_them_across_transactions() {
    let storage = SqliteStorage::open(memory_database().await).await.unwrap();
    storage.commit(&[root()], &|_| {}, ctx()).await.unwrap();
    let writes: Vec<_> = (0..100).map(|index| entry(index + 2, "cached")).collect();
    storage.commit(&writes, &|_| {}, ctx()).await.unwrap();
    storage.commit(&[entry(102, "cached-again")], &|_| {}, ctx()).await.unwrap();
    assert_eq!(storage.entry(EntryId(2), ctx()).await.unwrap().unwrap().entry.kind, "cached");
    assert_eq!(storage.entry(EntryId(102), ctx()).await.unwrap().unwrap().entry.kind, "cached-again");
    let error = storage.commit(&[root()], &|_| {}, ctx()).await.unwrap_err();
    assert!(error.to_string().contains("ID 1 already belongs to conversation"));
    assert_eq!(storage.entry(EntryId(2), ctx()).await.unwrap().unwrap().entry.kind, "cached");
    storage.close(ctx()).await.unwrap();
}

#[tokio::test]
async fn commits_work_through_the_transaction_handle_and_closes_idempotently() {
    let database = memory_database().await;
    database
        .transaction(Box::new(|handle| {
            Box::pin(async move {
                handle.exec("CREATE TABLE async_probe (value INTEGER)").await?;
                handle.run("INSERT INTO async_probe (value) VALUES (?)", &[SqliteValue::Integer(1)]).await
            })
        }))
        .await
        .unwrap();
    let row = database.get("SELECT value FROM async_probe", &[]).await.unwrap().unwrap();
    assert_eq!(row.integer("value").unwrap(), 1);
    database.close().await.unwrap();
    database.close().await.unwrap();
}

#[tokio::test]
async fn serializes_concurrent_transactions() {
    let database = memory_database().await;
    database.exec("CREATE TABLE transaction_queue (value INTEGER)").await.unwrap();
    let (started_tx, started) = oneshot::channel();
    let (release, gate) = oneshot::channel::<()>();
    let first_db = database.clone();
    let first = tokio::spawn(async move {
        first_db
            .transaction(Box::new(move |handle| {
                Box::pin(async move {
                    handle.exec("INSERT INTO transaction_queue (value) VALUES (1)").await?;
                    started_tx.send(()).unwrap();
                    gate.await.unwrap();
                    Ok(())
                })
            }))
            .await
    });
    started.await.unwrap();
    let second_started = Arc::new(AtomicBool::new(false));
    let (second_db, flag) = (database.clone(), second_started.clone());
    let second = tokio::spawn(async move {
        second_db
            .transaction(Box::new(move |handle| {
                Box::pin(async move {
                    flag.store(true, Ordering::SeqCst);
                    handle.exec("INSERT INTO transaction_queue (value) VALUES (2)").await
                })
            }))
            .await
    });
    yield_now().await;
    assert!(!second_started.load(Ordering::SeqCst));
    release.send(()).unwrap();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    let rows = database.all("SELECT value FROM transaction_queue ORDER BY value", &[]).await.unwrap();
    assert_eq!(values(&rows), [1, 2]);
    database.close().await.unwrap();
}

async fn rolled_back_transaction(database: Arc<BundledSqliteDatabase>, table: &'static str) -> DurableResult<()> {
    database
        .transaction(Box::new(move |handle| {
            Box::pin(async move {
                handle.run(&format!("INSERT INTO {table} (value) VALUES (?)"), &[SqliteValue::Integer(1)]).await?;
                yield_now().await;
                Err(DurableError::error("roll back"))
            })
        }))
        .await
}

#[tokio::test]
async fn runs_operations_in_call_order_whether_they_start_immediately_or_wait() {
    let database = memory_database().await;
    database.exec("CREATE TABLE call_order (value INTEGER)").await.unwrap();
    let transaction = tokio::spawn(rolled_back_transaction(database.clone(), "call_order"));
    let sql = "SELECT value FROM call_order ORDER BY value";
    let (before_db, write_db, after_db) = (database.clone(), database.clone(), database.clone());
    let before = tokio::spawn(async move { before_db.all(sql, &[]).await });
    let write = tokio::spawn(async move {
        write_db.run("INSERT INTO call_order (value) VALUES (?)", &[SqliteValue::Integer(2)]).await
    });
    let after = tokio::spawn(async move { after_db.all(sql, &[]).await });
    let error = transaction.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("roll back"));
    write.await.unwrap().unwrap();
    assert_eq!(values(&before.await.unwrap().unwrap()), Vec::<i64>::new());
    assert_eq!(values(&after.await.unwrap().unwrap()), [2]);

    let storage = Arc::new(SqliteStorage::open(database).await.unwrap());
    let committer = storage.clone();
    let commit = tokio::spawn(async move { committer.commit(&[root()], &|_| {}, ctx()).await });
    let reader = storage.clone();
    let read = tokio::spawn(async move { reader.conversation(ROOT_CONVERSATION_ID, ctx()).await });
    commit.await.unwrap().unwrap();
    let found = read.await.unwrap().unwrap();
    assert_eq!(found, Some(ConversationRecord { id: ROOT_CONVERSATION_ID, parent: None, owner: None }));
    storage.close(ctx()).await.unwrap();
}

#[tokio::test]
async fn queues_ordinary_operations_behind_an_active_transaction() {
    let database = memory_database().await;
    database.exec("CREATE TABLE operation_queue (value INTEGER)").await.unwrap();
    let (started_tx, started) = oneshot::channel();
    let (release, gate) = oneshot::channel::<()>();
    let pending_db = database.clone();
    let pending = tokio::spawn(async move {
        pending_db
            .transaction(Box::new(move |handle| {
                Box::pin(async move {
                    handle.exec("INSERT INTO operation_queue (value) VALUES (1)").await?;
                    started_tx.send(()).unwrap();
                    gate.await.unwrap();
                    Ok(())
                })
            }))
            .await
    });
    started.await.unwrap();
    let (write_db, read_db) = (database.clone(), database.clone());
    let write = tokio::spawn(async move { write_db.exec("INSERT INTO operation_queue (value) VALUES (2)").await });
    let read = tokio::spawn(async move { read_db.all("SELECT value FROM operation_queue ORDER BY value", &[]).await });
    yield_now().await;
    assert!(!write.is_finished());
    assert!(!read.is_finished());
    release.send(()).unwrap();
    pending.await.unwrap().unwrap();
    write.await.unwrap().unwrap();
    assert_eq!(values(&read.await.unwrap().unwrap()), [1, 2]);
    database.close().await.unwrap();
}

#[tokio::test]
async fn queues_database_calls_a_transaction_makes_on_the_database_itself() {
    let database = memory_database().await;
    database.exec("CREATE TABLE barrier_probe (value INTEGER)").await.unwrap();
    let outside = Arc::new(Mutex::new(None));
    let (inner_db, slot) = (database.clone(), outside.clone());
    let result = database
        .transaction(Box::new(move |handle| {
            Box::pin(async move {
                // Misuse: this call must wait for the transaction instead of joining it.
                let call = tokio::spawn(async move {
                    inner_db.run("INSERT INTO barrier_probe (value) VALUES (?)", &[SqliteValue::Integer(2)]).await
                });
                *slot.lock().unwrap() = Some(call);
                handle.run("INSERT INTO barrier_probe (value) VALUES (?)", &[SqliteValue::Integer(1)]).await?;
                yield_now().await;
                Err(DurableError::error("roll back"))
            })
        }))
        .await;
    assert!(result.unwrap_err().to_string().contains("roll back"));
    let call = outside.lock().unwrap().take().unwrap();
    call.await.unwrap().unwrap();
    assert_eq!(values(&database.all("SELECT value FROM barrier_probe", &[]).await.unwrap()), [2]);
    database.close().await.unwrap();
}

#[tokio::test]
async fn reports_the_rows_a_statement_changed_without_index_writes() {
    let database = memory_database().await;
    assert_eq!(changes::run_changes(&*database).await.unwrap(), changes::RUN_CHANGES);
    let in_transaction = pi_durable::storage::sqlite::transaction(&*database, |handle| {
        Box::pin(async move {
            let row = [SqliteValue::Text("t".into()), SqliteValue::Text("t/1".into()), SqliteValue::Integer(1)];
            handle.run_counted("INSERT INTO changes_probe VALUES (?, ?, ?)", &row).await
        })
    })
    .await
    .unwrap();
    assert_eq!(in_transaction, 1);
    assert_eq!(changes::trigger_changes(&*database).await.unwrap(), (1, 1));
    database.close().await.unwrap();
}

#[path = "support/sqlite_changes.rs"]
mod changes;

#[path = "support/sqlite_facade_settlement.rs"]
mod settlement;
