//! Facade cases about close, stale handles, rollback failures, and delayed settlement.

use super::*;

#[tokio::test]
async fn lets_admitted_multi_query_reads_finish_before_storage_closes() {
    let storage = Arc::new(SqliteStorage::open(memory_database().await).await.unwrap());
    storage.commit(&[root(), entry(2, "probe")], ctx()).await.unwrap();
    let (scan_storage, entry_storage, head_storage) = (storage.clone(), storage.clone(), storage.clone());
    let scan = tokio::spawn(async move {
        scan_storage.scan_entries(&EntryQuery::new(ROOT_CONVERSATION_ID), 10, None, ctx()).await
    });
    let visible =
        tokio::spawn(async move { entry_storage.visible_entry(ROOT_CONVERSATION_ID, EntryId(2), ctx()).await });
    let head =
        tokio::spawn(async move { head_storage.find_latest_head_marker(ROOT_CONVERSATION_ID, None, ctx()).await });
    yield_now().await;
    let (first, second) = tokio::join!(storage.close(ctx()), storage.close(ctx()));
    first.unwrap();
    second.unwrap();
    let ids: Vec<_> = scan.await.unwrap().unwrap().items.iter().map(|entry| entry.id).collect();
    assert_eq!(ids, [EntryId(2)]);
    assert_eq!(visible.await.unwrap().unwrap().unwrap().entry.kind, "probe");
    assert_eq!(head.await.unwrap().unwrap(), None);
    let error = storage.scan_entries(&EntryQuery::new(ROOT_CONVERSATION_ID), 10, None, ctx()).await.unwrap_err();
    assert!(error.to_string().contains("SqliteStorage is closed"));
}

#[tokio::test]
async fn rejects_a_transaction_handle_used_after_its_transaction_settles() {
    let database = memory_database().await;
    database.exec("CREATE TABLE stale_probe (value INTEGER)").await.unwrap();
    let kept: Arc<Mutex<Option<Arc<dyn SqliteExecutor>>>> = Arc::new(Mutex::new(None));
    let slot = kept.clone();
    database
        .transaction(Box::new(move |handle| {
            Box::pin(async move {
                *slot.lock().unwrap() = Some(handle.clone());
                handle.run("INSERT INTO stale_probe (value) VALUES (?)", &[SqliteValue::Integer(1)]).await
            })
        }))
        .await
        .unwrap();
    let handle = kept.lock().unwrap().take().unwrap();
    let stale = "SQLite transaction handle is no longer active";
    assert!(handle.exec("INSERT INTO stale_probe (value) VALUES (2)").await.unwrap_err().to_string().contains(stale));
    let run = handle.run("INSERT INTO stale_probe (value) VALUES (?)", &[SqliteValue::Integer(3)]).await;
    assert!(run.unwrap_err().to_string().contains(stale));
    assert_eq!(values(&database.all("SELECT value FROM stale_probe", &[]).await.unwrap()), [1]);
    database.close().await.unwrap();
}

#[tokio::test]
async fn does_not_keep_the_callback_error_when_rollback_itself_fails() {
    let database = memory_database().await;
    database.exec("CREATE TABLE rollback_probe (value INTEGER)").await.unwrap();
    let callback_error = "rejected after an escaped commit";
    let result = database
        .transaction(Box::new(move |handle| {
            Box::pin(async move {
                handle.exec("INSERT INTO rollback_probe (value) VALUES (1)").await?;
                handle.exec("COMMIT").await?;
                Err(DurableError::error(callback_error))
            })
        }))
        .await;
    let error = result.unwrap_err();
    assert_ne!(error.to_string(), callback_error);
    assert!(error.to_string().contains("rollback failed"), "{error}");
    let row = database.get("SELECT value FROM rollback_probe", &[]).await.unwrap().unwrap();
    assert_eq!(row.integer("value").unwrap(), 1);
    database.close().await.unwrap();
}

#[derive(Clone, Copy, PartialEq)]
enum Settlement {
    Immediate,
    Delay,
    Reject,
}

/// Delays when a transaction's result reaches the caller, and can turn a success into a failure.
struct ControlledSettlementDatabase {
    delegate: Arc<BundledSqliteDatabase>,
    mode: Mutex<Settlement>,
    settle: Notify,
}

impl ControlledSettlementDatabase {
    fn control_next_settlement(&self, mode: Settlement) {
        *self.mode.lock().unwrap() = mode;
    }

    fn settle(&self) {
        self.settle.notify_one();
    }
}

#[async_trait]
impl SqliteExecutor for ControlledSettlementDatabase {
    async fn exec(&self, sql: &str) -> DurableResult<()> {
        self.delegate.exec(sql).await
    }
    async fn run(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<()> {
        self.delegate.run(sql, params).await
    }
    async fn get(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Option<SqliteRow>> {
        self.delegate.get(sql, params).await
    }
    async fn all(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Vec<SqliteRow>> {
        self.delegate.all(sql, params).await
    }
}

#[async_trait]
impl SqliteDatabase for ControlledSettlementDatabase {
    async fn transaction<'a>(&'a self, callback: TransactionCallback<'a>) -> DurableResult<()> {
        let mode = std::mem::replace(&mut *self.mode.lock().unwrap(), Settlement::Immediate);
        if mode == Settlement::Immediate {
            return self.delegate.transaction(callback).await;
        }
        let settlement = self
            .delegate
            .transaction(Box::new(move |handle| {
                Box::pin(async move {
                    callback(handle).await?;
                    if mode == Settlement::Reject {
                        return Err(DurableError::error("controlled settlement rejection"));
                    }
                    Ok(())
                })
            }))
            .await;
        self.settle.notified().await;
        settlement
    }

    async fn close(&self) -> DurableResult<()> {
        self.delegate.close().await
    }
}

#[tokio::test]
async fn awaits_async_transaction_settlement_and_adopts_ids_only_after_success() {
    let database = Arc::new(ControlledSettlementDatabase {
        delegate: memory_database().await,
        mode: Mutex::new(Settlement::Immediate),
        settle: Notify::new(),
    });
    database.control_next_settlement(Settlement::Delay);
    let opening = tokio::spawn(SqliteStorage::open(database.clone()));
    yield_now().await;
    assert!(!opening.is_finished());
    database.settle();
    let storage = Arc::new(opening.await.unwrap().unwrap());

    database.control_next_settlement(Settlement::Delay);
    let committer = storage.clone();
    let committing = tokio::spawn(async move { committer.commit(&[entry(100, "settled")], ctx()).await });
    yield_now().await;
    assert!(!committing.is_finished());
    assert_eq!(storage.mint_id().await.unwrap(), 2);
    database.settle();
    assert_eq!(committing.await.unwrap().unwrap(), Seq(1));
    assert_eq!(storage.mint_id().await.unwrap(), 101);

    database.control_next_settlement(Settlement::Reject);
    let committer = storage.clone();
    let rejected = tokio::spawn(async move { committer.commit(&[entry(200, "rejected")], ctx()).await });
    yield_now().await;
    assert_eq!(storage.mint_id().await.unwrap(), 102);
    database.settle();
    let error = rejected.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("controlled settlement rejection"));
    assert_eq!(storage.mint_id().await.unwrap(), 103);
    assert_eq!(storage.entry(EntryId(200), ctx()).await.unwrap(), None);
    storage.close(ctx()).await.unwrap();
}

#[tokio::test]
async fn reads_a_document_from_one_committed_state_while_a_commit_replaces_its_base() {
    let id = DocumentId(5);
    // Each yield count starts the commit at a different point of the read's record and revision queries.
    for yields in 0..16 {
        let storage = Arc::new(SqliteStorage::open(memory_database().await).await.unwrap());
        let record = DocumentCreate {
            id,
            kind: "replaced".into(),
            key: None,
            scope: DocumentScope::Session,
            history: None,
            fork: None,
        };
        let content = DocumentContent::Base { version: 1, value: Json::parse(r#"{"value":1}"#).unwrap() };
        storage.commit(&[StorageWrite::DocumentCreate { record, content }], ctx()).await.unwrap();
        let reader = storage.clone();
        let read = tokio::spawn(async move { reader.document(id, DocumentPoint::Current, ctx()).await });
        for _ in 0..yields {
            yield_now().await;
        }
        let replace = StorageWrite::DocumentChange {
            id,
            content: DocumentContent::Base { version: 1, value: Json::parse(r#"{"value":2}"#).unwrap() },
        };
        storage.commit(&[replace], ctx()).await.unwrap();
        let stored = read.await.unwrap().unwrap().unwrap().value;
        assert!(stored == Json::parse(r#"{"value":1}"#).unwrap() || stored == Json::parse(r#"{"value":2}"#).unwrap());
        storage.close(ctx()).await.unwrap();
    }
}
