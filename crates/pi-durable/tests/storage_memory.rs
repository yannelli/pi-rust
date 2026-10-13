//! Port of upstream `test/storage-memory.ts` as an `#[ignore]`d report (`-- --ignored --nocapture`). Upstream measures
//! the V8 heap after forced GC; Rust has no collected heap, so this reports resident memory from `/proc/self/statm`
//! where it exists, plus the same disk footprint.

use std::path::Path;
use std::sync::Arc;

use pi_chord::BACKGROUND_CONTEXT;
use pi_durable::storage::MemoryStorage;
use pi_durable::storage::jsonl::{JsonlStorageOptions, open_local_jsonl_storage};
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::testing::storage_benchmark::*;
use pi_durable::*;
use rusqlite::Connection;

fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

fn mebibytes(bytes: u64) -> String {
    format!("{:.2}", bytes as f64 / 1024.0 / 1024.0)
}

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn disk(backend: &str, path: &Path) -> String {
    match backend {
        "sqlite" => {
            let auxiliary = size(&path.with_extension("sqlite-wal")) + size(&path.with_extension("sqlite-shm"));
            let connection = Connection::open(path).unwrap();
            let count = |sql: &str| connection.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();
            let pages = count("SELECT page_count FROM pragma_page_count()");
            let free = count("SELECT freelist_count FROM pragma_freelist_count()");
            format!(
                "main {} MiB, auxiliary {} MiB, pages/free {pages}/{free}",
                mebibytes(size(path)),
                mebibytes(auxiliary)
            )
        }
        "jsonl" => {
            let (mut main, mut auxiliary, mut documents, mut tasks) = (0, 0, 0, 0);
            for entry in std::fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                let bytes = size(&entry.path());
                if name == "main.jsonl" {
                    main += bytes;
                } else {
                    auxiliary += bytes;
                }
                documents += usize::from(name.starts_with("doc-"));
                tasks += usize::from(name.starts_with("task-"));
            }
            format!(
                "main {} MiB, auxiliary {} MiB, documents/tasks {documents}/{tasks}",
                mebibytes(main),
                mebibytes(auxiliary)
            )
        }
        _ => "-".into(),
    }
}

fn delta(after: Option<u64>, before: Option<u64>) -> String {
    match (after, before) {
        (Some(after), Some(before)) => mebibytes(after.saturating_sub(before)),
        _ => "-".into(),
    }
}

#[tokio::test]
#[ignore = "footprint report; run with --ignored --nocapture"]
async fn storage_footprint() {
    println!("Storage footprint after deterministic synthetic workloads; values are process deltas, not limits.");
    for backend in ["memory", "sqlite", "jsonl"] {
        for scale in STORAGE_MEMORY_SCALES {
            let directory = tempfile::tempdir().unwrap();
            let name = if backend == "sqlite" { "storage.sqlite" } else { "storage" };
            let path = directory.path().join(name);
            let text = path.to_string_lossy().into_owned();
            let baseline = resident_bytes();
            let storage: Arc<dyn Storage> = match backend {
                "memory" => Arc::new(MemoryStorage::new()),
                "sqlite" => Arc::new(open_sqlite_storage(&text).await.unwrap()),
                _ => {
                    let options = JsonlStorageOptions::default();
                    Arc::new(open_local_jsonl_storage(&text, &BACKGROUND_CONTEXT, options).await.unwrap())
                }
            };
            let dataset = seed_storage_benchmark(&*storage, scale).await.unwrap();
            let seeded = resident_bytes();
            let mut checksum = 0_u64;
            for _ in 0..10 {
                for scenario in storage_read_benchmarks() {
                    let result = (scenario.run)(&*storage, &dataset).await;
                    assert_eq!(result, (scenario.expected)(&dataset), "{}", scenario.name);
                    checksum = checksum.wrapping_add(result);
                }
            }
            let read = resident_bytes();
            let records = storage_benchmark_primary_record_count(scale) as u64;
            let per_record = match (seeded, baseline) {
                (Some(after), Some(before)) => (after.saturating_sub(before) / records).to_string(),
                _ => "-".into(),
            };
            println!(
                "{backend:<7} {:<5} RSS after seed {} MiB, after reads {} MiB, bytes/record {per_record}, disk {} ({checksum})",
                scale.name,
                delta(seeded, baseline),
                delta(read, seeded),
                disk(backend, &path),
            );
            storage.close(&BACKGROUND_CONTEXT).await.unwrap();
        }
    }
}
