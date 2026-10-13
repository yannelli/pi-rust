//! Port of upstream `test/storage.bench.ts`. The scenario check always runs; the timing run is `#[ignore]`d:
//! `cargo test -p pi-durable --release --test storage_bench -- --ignored --nocapture`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pi_chord::BACKGROUND_CONTEXT;
use pi_durable::storage::MemoryStorage;
use pi_durable::storage::jsonl::{JsonlStorageOptions, open_local_jsonl_storage};
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::testing::storage_benchmark::*;
use pi_durable::*;

const BACKENDS: [&str; 3] = ["memory", "sqlite", "jsonl"];

fn storage_path(directory: &Path, backend: &str) -> String {
    let name = if backend == "sqlite" { "storage.sqlite" } else { "storage" };
    directory.join(name).to_string_lossy().into_owned()
}

async fn open_backend(backend: &str, path: &str) -> Arc<dyn Storage> {
    match backend {
        "memory" => Arc::new(MemoryStorage::new()),
        "sqlite" => Arc::new(open_sqlite_storage(path).await.unwrap()),
        _ => {
            Arc::new(open_local_jsonl_storage(path, &BACKGROUND_CONTEXT, JsonlStorageOptions::default()).await.unwrap())
        }
    }
}

async fn fresh(backend: &str) -> (tempfile::TempDir, Arc<dyn Storage>) {
    let directory = tempfile::tempdir().unwrap();
    let storage = open_backend(backend, &storage_path(directory.path(), backend)).await;
    (directory, storage)
}

#[tokio::test]
async fn every_benchmark_scenario_returns_its_expected_result_on_every_backend() {
    for backend in BACKENDS {
        let (_directory, storage) = fresh(backend).await;
        let dataset = seed_storage_benchmark(&*storage, TIMING_SCALE).await.unwrap();
        for scenario in storage_read_benchmarks() {
            assert_eq!(
                (scenario.run)(&*storage, &dataset).await,
                (scenario.expected)(&dataset),
                "{backend}: {}",
                scenario.name
            );
        }
        for (index, (name, expected)) in STORAGE_WRITE_BENCHMARKS.into_iter().enumerate() {
            let (_directory, storage) = fresh(backend).await;
            seed_storage_write_benchmark(&*storage).await.unwrap();
            assert_eq!(run_storage_write_benchmark(&*storage, index).await.unwrap(), expected, "{backend}: {name}");
        }
    }
}

fn report(group: &str, backend: &str, samples: &[Duration]) {
    let mut sorted = samples.to_vec();
    sorted.sort();
    let mean = sorted.iter().sum::<Duration>() / sorted.len().max(1) as u32;
    println!("{group:<48} {backend:<7} mean {mean:>12?}  median {:>12?}  n={}", sorted[sorted.len() / 2], sorted.len());
}

#[tokio::test]
#[ignore = "timing benchmark; run with --ignored --nocapture"]
async fn storage_benchmarks() {
    let mut readers = Vec::new();
    for backend in BACKENDS {
        let (directory, storage) = fresh(backend).await;
        let dataset = seed_storage_benchmark(&*storage, TIMING_SCALE).await.unwrap();
        readers.push((backend, directory, storage, dataset));
    }
    for scenario in storage_read_benchmarks() {
        for (backend, _, storage, dataset) in &readers {
            let mut samples = Vec::new();
            let started = Instant::now();
            for round in 0.. {
                let begin = Instant::now();
                assert_eq!((scenario.run)(&**storage, dataset).await, (scenario.expected)(dataset));
                if round >= 3 {
                    samples.push(begin.elapsed());
                }
                if samples.len() >= 10 && started.elapsed() > Duration::from_millis(300) {
                    break;
                }
            }
            report(&scenario.name, backend, &samples);
        }
    }

    for (index, (name, expected)) in STORAGE_WRITE_BENCHMARKS.into_iter().enumerate() {
        for backend in BACKENDS {
            let mut samples = Vec::new();
            for round in 0..25 {
                let (_directory, storage) = fresh(backend).await;
                seed_storage_write_benchmark(&*storage).await.unwrap();
                let begin = Instant::now();
                assert_eq!(run_storage_write_benchmark(&*storage, index).await.unwrap(), expected);
                if round >= 5 {
                    samples.push(begin.elapsed());
                }
            }
            report(name, backend, &samples);
        }
    }

    for backend in ["sqlite", "jsonl"] {
        let seed_directory = tempfile::tempdir().unwrap();
        let seed_path = storage_path(seed_directory.path(), backend);
        let seed = open_backend(backend, &seed_path).await;
        let dataset = seed_storage_benchmark(&*seed, TIMING_SCALE).await.unwrap();
        seed.close(&BACKGROUND_CONTEXT).await.unwrap();
        let mut samples = Vec::new();
        for round in 0..23 {
            let sample = tempfile::tempdir().unwrap();
            let path = storage_path(sample.path(), backend);
            copy_storage(Path::new(&seed_path), Path::new(&path));
            let begin = Instant::now();
            let storage = open_backend(backend, &path).await;
            let found = storage.entry(dataset.first_entry_id, &BACKGROUND_CONTEXT).await.unwrap();
            if round >= 3 {
                samples.push(begin.elapsed());
            }
            assert_eq!(found.map(|stored| stored.entry.id), Some(dataset.first_entry_id));
            storage.close(&BACKGROUND_CONTEXT).await.unwrap();
        }
        report("reopen and first exact read", backend, &samples);
    }
}

fn copy_storage(source: &Path, destination: &Path) {
    if source.is_file() {
        std::fs::copy(source, destination).unwrap();
        return;
    }
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
    }
}
