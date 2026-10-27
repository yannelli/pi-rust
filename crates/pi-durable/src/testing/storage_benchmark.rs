//! Deterministic storage benchmark workloads that run only through the public `Storage` contract.

use futures::future::BoxFuture;
use pi_chord::delta::Op;
use pi_chord::{BACKGROUND_CONTEXT, Context, Json};

use crate::errors::DurableResult;
use crate::ids::{ConversationId, DocumentId, EntryId, ROOT_CONVERSATION_ID, Seq, SubmissionId, TaskId};
use crate::types::{
    ConversationParent, ConversationRecord, DocumentAddress, DocumentContent, DocumentCreate, DocumentFork,
    DocumentHistory, DocumentPoint, DocumentScope, EntryQuery, EntryRecord, Storage, StorageWrite, SubmissionRecord,
    SubmissionStatus, SubmissionType, TaskOutcome, TaskQuery, TaskRecord, TaskState, TaskStatus,
};

/// Size of one seeded workload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageBenchmarkScale {
    pub name: &'static str,
    pub entry_count: usize,
    pub task_count: usize,
    pub document_count: usize,
}

pub const STORAGE_MEMORY_SCALES: [StorageBenchmarkScale; 2] = [
    StorageBenchmarkScale { name: "1k", entry_count: 1_000, task_count: 200, document_count: 200 },
    StorageBenchmarkScale { name: "10k", entry_count: 10_000, task_count: 2_000, document_count: 2_000 },
];

pub const TIMING_SCALE: StorageBenchmarkScale =
    StorageBenchmarkScale { name: "timing", entry_count: 1_000, task_count: 300, document_count: 300 };

pub const REPLAY_TAILS: [u64; 4] = [0, 16, 128, 1_024];
const HISTORY_SEGMENT_LENGTH: u64 = 128;
const FORK_DEPTH: u64 = 8;
const ENTRIES_PER_FORK: usize = 32;
const BATCH_SIZE: usize = 100;

/// Records a seeded workload holds: the root, entries, tasks, documents, replay and history documents, and forks.
pub fn storage_benchmark_primary_record_count(scale: StorageBenchmarkScale) -> usize {
    1 + scale.entry_count
        + scale.task_count
        + scale.document_count
        + REPLAY_TAILS.len()
        + 1
        + FORK_DEPTH as usize * (1 + ENTRIES_PER_FORK)
}

/// IDs and points of a seeded workload that the read benchmarks query.
#[derive(Clone, Debug, PartialEq)]
pub struct StorageBenchmarkDataset {
    pub first_entry_id: EntryId,
    pub filtered_task_count: u64,
    pub exact_document_id: DocumentId,
    pub exact_document_key: String,
    /// Replay document per tail length, in `REPLAY_TAILS` order.
    pub replay_document_ids: Vec<DocumentId>,
    pub historical_document_id: DocumentId,
    pub ancient_at: Seq,
    pub recent_at: Seq,
    pub deepest_conversation_id: ConversationId,
    pub ancestor_head_entry_id: EntryId,
}

fn ctx() -> &'static Context {
    &BACKGROUND_CONTEXT
}

fn json(value: serde_json::Value) -> Json {
    Json::from(value)
}

fn entry(id: EntryId, conversation_id: ConversationId, kind: &str, data: Option<Json>) -> EntryRecord {
    EntryRecord { kind: kind.into(), model: None, data, edits: None, id, conversation_id, head: None, by_task_id: None }
}

fn task(id: TaskId, index: u64) -> TaskRecord {
    let state = match index % 3 {
        0 => TaskState::Pending { checkpoint: json(serde_json::json!({ "index": index, "payload": "x".repeat(64) })) },
        1 => TaskState::Running { checkpoint: json(serde_json::json!({ "index": index, "payload": "x".repeat(64) })) },
        _ => TaskState::Terminal { outcome: TaskOutcome::completed(json(serde_json::json!({ "index": index }))) },
    };
    TaskRecord {
        id,
        conversation_id: ROOT_CONVERSATION_ID,
        kind: if index % 4 == 0 { "benchmark.filtered" } else { "benchmark.other" }.into(),
        version: 1,
        input: json(serde_json::json!({ "index": index })),
        owner: None,
        background: index % 5 == 0,
        abort_requested: index % 7 == 0,
        abort_reason: None,
        abandon_on_restart: None,
        state,
        memos: None,
        started_at: None,
        ended_at: None,
        restarts: None,
        trace: Default::default(),
        links: Default::default(),
    }
}

fn set_count(id: DocumentId, count: u64) -> StorageWrite {
    let ops = vec![Op::Set(vec!["count".into()], Json::from(count))];
    StorageWrite::DocumentChange { id, content: DocumentContent::Delta { version: 1, ops } }
}

fn root_document(id: DocumentId, kind: &str, key: Option<String>) -> DocumentCreate {
    DocumentCreate {
        id,
        kind: kind.into(),
        key,
        scope: DocumentScope::Conversation { conversation_id: ROOT_CONVERSATION_ID },
        history: Some(DocumentHistory::Rewindable),
        fork: Some(DocumentFork::AsOf),
    }
}

async fn mint<T: From<u64>>(storage: &dyn Storage) -> DurableResult<T> {
    Ok(T::from(storage.mint_id().await?))
}

async fn commit_root(storage: &dyn Storage) -> DurableResult<Seq> {
    let root = ConversationRecord { id: ROOT_CONVERSATION_ID, parent: None, owner: None };
    storage.commit(&[StorageWrite::Conversation { value: root }], &|_| {}, ctx()).await
}

/// Seed deterministic representative data through only the public `Storage` contract.
pub async fn seed_storage_benchmark(
    storage: &dyn Storage,
    scale: StorageBenchmarkScale,
) -> DurableResult<StorageBenchmarkDataset> {
    commit_root(storage).await?;
    let mut first_entry_id = None;
    for start in (0..scale.entry_count).step_by(BATCH_SIZE) {
        let mut writes = Vec::new();
        for index in start..(start + BATCH_SIZE).min(scale.entry_count) {
            let id: EntryId = mint(storage).await?;
            let data = json(serde_json::json!({ "index": index, "text": format!("entry-{index}-{}", "x".repeat(96)) }));
            let mut value = entry(id, ROOT_CONVERSATION_ID, "benchmark.entry", Some(data));
            if index == 0 {
                first_entry_id = Some(id);
                value.head = Some(id);
            }
            writes.push(StorageWrite::Entry { value });
        }
        storage.commit(&writes, &|_| {}, ctx()).await?;
    }
    for start in (0..scale.task_count).step_by(BATCH_SIZE) {
        let mut writes = Vec::new();
        for index in start..(start + BATCH_SIZE).min(scale.task_count) {
            writes.push(StorageWrite::Task { value: task(mint(storage).await?, index as u64) });
        }
        storage.commit(&writes, &|_| {}, ctx()).await?;
    }
    let mut exact_document_id = DocumentId(0);
    for start in (0..scale.document_count).step_by(BATCH_SIZE) {
        let mut writes = Vec::new();
        for index in start..(start + BATCH_SIZE).min(scale.document_count) {
            exact_document_id = mint(storage).await?;
            let record = DocumentCreate {
                id: exact_document_id,
                kind: "benchmark.family".into(),
                key: Some(format!("key-{index}")),
                scope: DocumentScope::Session,
                history: None,
                fork: None,
            };
            let value = json(serde_json::json!({ "index": index, "text": "x".repeat(128) }));
            writes.push(StorageWrite::DocumentCreate { record, content: DocumentContent::Base { version: 1, value } });
        }
        storage.commit(&writes, &|_| {}, ctx()).await?;
    }

    let mut replay_document_ids = Vec::new();
    for _ in REPLAY_TAILS {
        replay_document_ids.push(mint::<DocumentId>(storage).await?);
    }
    let creates: Vec<_> = REPLAY_TAILS
        .iter()
        .zip(&replay_document_ids)
        .map(|(tail, id)| StorageWrite::DocumentCreate {
            record: root_document(*id, "benchmark.replay", Some(tail.to_string())),
            content: DocumentContent::Base {
                version: 1,
                value: json(serde_json::json!({ "count": 0, "text": "x".repeat(64) })),
            },
        })
        .collect();
    storage.commit(&creates, &|_| {}, ctx()).await?;
    for count in 1..=REPLAY_TAILS[REPLAY_TAILS.len() - 1] {
        let writes: Vec<_> = REPLAY_TAILS
            .iter()
            .zip(&replay_document_ids)
            .filter(|(tail, _)| count <= **tail)
            .map(|(_, id)| set_count(*id, count))
            .collect();
        storage.commit(&writes, &|_| {}, ctx()).await?;
    }

    let historical_document_id: DocumentId = mint(storage).await?;
    let record = root_document(historical_document_id, "benchmark.history", None);
    let base = |count: u64| DocumentContent::Base { version: 1, value: json(serde_json::json!({ "count": count })) };
    storage.commit(&[StorageWrite::DocumentCreate { record, content: base(0) }], &|_| {}, ctx()).await?;
    let mut ancient_at = Seq(0);
    for count in 1..=HISTORY_SEGMENT_LENGTH {
        ancient_at = storage.commit(&[set_count(historical_document_id, count)], &|_| {}, ctx()).await?;
    }
    let checkpoint = StorageWrite::DocumentChange { id: historical_document_id, content: base(HISTORY_SEGMENT_LENGTH) };
    storage.commit(&[checkpoint], &|_| {}, ctx()).await?;
    let mut recent_at = ancient_at;
    for count in HISTORY_SEGMENT_LENGTH + 1..=HISTORY_SEGMENT_LENGTH * 2 {
        recent_at = storage.commit(&[set_count(historical_document_id, count)], &|_| {}, ctx()).await?;
    }

    let first_entry_id = first_entry_id.expect("a benchmark scale creates entries");
    let (mut parent, mut parent_at, mut deepest) = (ROOT_CONVERSATION_ID, first_entry_id, ROOT_CONVERSATION_ID);
    for depth in 0..FORK_DEPTH {
        let conversation_id: ConversationId = mint(storage).await?;
        let fork = ConversationRecord {
            id: conversation_id,
            parent: Some(ConversationParent { conversation_id: parent, at: parent_at }),
            owner: None,
        };
        storage.commit(&[StorageWrite::Conversation { value: fork }], &|_| {}, ctx()).await?;
        let mut writes = Vec::new();
        for index in 0..ENTRIES_PER_FORK {
            let data = json(serde_json::json!({ "depth": depth, "index": index }));
            let value = entry(mint(storage).await?, conversation_id, "benchmark.fork", Some(data));
            parent_at = value.id;
            writes.push(StorageWrite::Entry { value });
        }
        storage.commit(&writes, &|_| {}, ctx()).await?;
        parent = conversation_id;
        deepest = conversation_id;
    }

    Ok(StorageBenchmarkDataset {
        first_entry_id,
        filtered_task_count: 50.min(scale.task_count.div_ceil(60) as u64),
        exact_document_id,
        exact_document_key: format!("key-{}", scale.document_count - 1),
        replay_document_ids,
        historical_document_id,
        ancient_at,
        recent_at,
        deepest_conversation_id: deepest,
        ancestor_head_entry_id: first_entry_id,
    })
}

/// Runs one read scenario and returns the number it checks.
pub type ReadRun =
    Box<dyn for<'a> Fn(&'a dyn Storage, &'a StorageBenchmarkDataset) -> BoxFuture<'a, u64> + Send + Sync>;

/// The number a read scenario must return for a dataset.
pub type ReadExpectation = Box<dyn Fn(&StorageBenchmarkDataset) -> u64 + Send + Sync>;

/// One read scenario; `run` returns a number the benchmark checks against `expected`.
pub struct StorageReadBenchmark {
    pub name: String,
    pub run: ReadRun,
    pub expected: ReadExpectation,
}

async fn count_at(storage: &dyn Storage, id: DocumentId, at: DocumentPoint) -> u64 {
    let stored = storage.document(id, at, ctx()).await.expect("benchmark document read");
    stored.and_then(|stored| stored.value["count"].as_u64()).unwrap_or(u64::MAX)
}

async fn entry_page(storage: &dyn Storage, conversation_id: ConversationId) -> u64 {
    let page = storage.scan_entries(&EntryQuery::new(conversation_id), 100, None, ctx()).await;
    page.expect("benchmark entry scan").items.len() as u64
}

/// Every read scenario of the upstream benchmark, in its order.
pub fn storage_read_benchmarks() -> Vec<StorageReadBenchmark> {
    let mut scenarios = vec![
        StorageReadBenchmark {
            name: "exact entry lookup".into(),
            run: Box::new(|storage, dataset| {
                Box::pin(async move {
                    let found = storage.entry(dataset.first_entry_id, ctx()).await.expect("benchmark entry read");
                    found.map_or(u64::MAX, |stored| stored.entry.id.0)
                })
            }),
            expected: Box::new(|dataset| dataset.first_entry_id.0),
        },
        StorageReadBenchmark {
            name: "entry page scan (100)".into(),
            run: Box::new(|storage, _| Box::pin(entry_page(storage, ROOT_CONVERSATION_ID))),
            expected: Box::new(|_| 100),
        },
        StorageReadBenchmark {
            name: "filtered task scan (50)".into(),
            run: Box::new(|storage, _| {
                Box::pin(async move {
                    let query = TaskQuery {
                        kind: Some("benchmark.filtered".into()),
                        status: Some(TaskStatus::Pending),
                        background: Some(true),
                        ..Default::default()
                    };
                    storage.scan_tasks(&query, 50, None, ctx()).await.expect("benchmark task scan").items.len() as u64
                })
            }),
            expected: Box::new(|dataset| dataset.filtered_task_count),
        },
        StorageReadBenchmark {
            name: "exact document address among many".into(),
            run: Box::new(|storage, dataset| {
                Box::pin(async move {
                    let address = DocumentAddress {
                        kind: "benchmark.family".into(),
                        scope: DocumentScope::Session,
                        key: Some(dataset.exact_document_key.clone()),
                    };
                    let found = storage.find_document(&address, DocumentPoint::Current, ctx()).await;
                    found.expect("benchmark document lookup").map_or(u64::MAX, |record| record.id.0)
                })
            }),
            expected: Box::new(|dataset| dataset.exact_document_id.0),
        },
    ];
    for (index, tail) in REPLAY_TAILS.into_iter().enumerate() {
        scenarios.push(StorageReadBenchmark {
            name: format!("document replay tail ({tail})"),
            run: Box::new(move |storage, dataset| {
                Box::pin(count_at(storage, dataset.replay_document_ids[index], DocumentPoint::Current))
            }),
            expected: Box::new(move |_| tail),
        });
    }
    scenarios.extend([
        StorageReadBenchmark {
            name: "ancient historical read before newer base".into(),
            run: Box::new(|storage, dataset| {
                Box::pin(count_at(storage, dataset.historical_document_id, DocumentPoint::At(dataset.ancient_at)))
            }),
            expected: Box::new(|_| HISTORY_SEGMENT_LENGTH),
        },
        StorageReadBenchmark {
            name: "recent historical read after newer base".into(),
            run: Box::new(|storage, dataset| {
                Box::pin(count_at(storage, dataset.historical_document_id, DocumentPoint::At(dataset.recent_at)))
            }),
            expected: Box::new(|_| HISTORY_SEGMENT_LENGTH * 2),
        },
        StorageReadBenchmark {
            name: "fork-depth history scan (100)".into(),
            run: Box::new(|storage, dataset| Box::pin(entry_page(storage, dataset.deepest_conversation_id))),
            expected: Box::new(|_| 100),
        },
        StorageReadBenchmark {
            name: "fork-depth head lookup".into(),
            run: Box::new(|storage, dataset| {
                Box::pin(async move {
                    let found = storage.find_latest_head_marker(dataset.deepest_conversation_id, None, ctx()).await;
                    found.expect("benchmark head lookup").map_or(u64::MAX, |entry| entry.id.0)
                })
            }),
            expected: Box::new(|dataset| dataset.ancestor_head_entry_id.0),
        },
    ]);
    scenarios
}

/// Seed the common state every write benchmark sample expects.
pub async fn seed_storage_write_benchmark(storage: &dyn Storage) -> DurableResult<()> {
    commit_root(storage).await?;
    let mut writes = Vec::new();
    for index in 0..100 {
        let data = json(serde_json::json!({ "index": index }));
        writes.push(StorageWrite::Entry {
            value: entry(mint(storage).await?, ROOT_CONVERSATION_ID, "benchmark.baseline", Some(data)),
        });
    }
    storage.commit(&writes, &|_| {}, ctx()).await.map(drop)
}

/// The write scenarios: `(name, expected writes)`; run one with [`run_storage_write_benchmark`].
pub const STORAGE_WRITE_BENCHMARKS: [(&str, u64); 3] =
    [("commit one entry", 1), ("commit 100 entries", 100), ("commit mixed entry/task/submission/document", 4)];

/// Run write scenario `index` of [`STORAGE_WRITE_BENCHMARKS`] once; returns the number of writes committed.
pub async fn run_storage_write_benchmark(storage: &dyn Storage, index: usize) -> DurableResult<u64> {
    let text = json(serde_json::json!({ "text": "x".repeat(128) }));
    let writes = match index {
        0 => vec![StorageWrite::Entry {
            value: entry(mint(storage).await?, ROOT_CONVERSATION_ID, "benchmark.write", Some(text)),
        }],
        1 => {
            let mut writes = Vec::new();
            for index in 0..100 {
                let data = json(serde_json::json!({ "index": index, "text": "x".repeat(128) }));
                let value = entry(mint(storage).await?, ROOT_CONVERSATION_ID, "benchmark.write", Some(data));
                writes.push(StorageWrite::Entry { value });
            }
            writes
        }
        _ => {
            let entry_id: EntryId = mint(storage).await?;
            let task_id: TaskId = mint(storage).await?;
            let submission_id: SubmissionId = mint(storage).await?;
            let document_id: DocumentId = mint(storage).await?;
            let submission = SubmissionRecord {
                conversation_id: ROOT_CONVERSATION_ID,
                kind: SubmissionType::Write,
                request_id: Some(format!("benchmark-{submission_id}")),
                status: SubmissionStatus::Done,
                entry: Some(entry_id),
                answer: None,
                reason: None,
                detail: None,
                id: submission_id,
                trace: Default::default(),
            };
            let record = DocumentCreate {
                id: document_id,
                kind: "benchmark.mixed".into(),
                key: Some(document_id.to_string()),
                scope: DocumentScope::Session,
                history: None,
                fork: None,
            };
            let value = json(serde_json::json!({ "entryId": entry_id.0, "taskId": task_id.0 }));
            vec![
                StorageWrite::Entry { value: entry(entry_id, ROOT_CONVERSATION_ID, "benchmark.mixed", None) },
                StorageWrite::Task { value: task(task_id, task_id.0) },
                StorageWrite::Submission { value: submission },
                StorageWrite::DocumentCreate { record, content: DocumentContent::Base { version: 1, value } },
            ]
        }
    };
    storage.commit(&writes, &|_| {}, ctx()).await?;
    Ok(writes.len() as u64)
}
