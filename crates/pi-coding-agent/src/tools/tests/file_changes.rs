//! The edit and write tools report their changes to a [`FileChangeRecorder`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::noop;
use crate::tools::{
    EditToolOptions, FileChangeRecorder, PriorContent, ToolsOptions, WriteToolOptions, create_tool_with_options,
    too_large_reason,
};

type Change = (PathBuf, Option<PriorContent>, Vec<u8>);

/// Wants every prior state and keeps every change.
struct Recorder {
    max: u64,
    changes: Mutex<Vec<Change>>,
}

impl FileChangeRecorder for Recorder {
    fn wants_prior(&self, _path: &Path) -> bool {
        true
    }

    fn max_prior_bytes(&self) -> u64 {
        self.max
    }

    fn record(&self, path: &Path, prior: Option<PriorContent>, written: &[u8]) {
        self.changes.lock().unwrap().push((path.to_path_buf(), prior, written.to_vec()));
    }
}

async fn run(dir: &Path, max: u64, name: &str, args: Value) -> (anyhow::Result<()>, Vec<Change>) {
    let recorder = Arc::new(Recorder { max, changes: Mutex::default() });
    let options = ToolsOptions {
        write: WriteToolOptions { recorder: Some(recorder.clone()), ..Default::default() },
        edit: EditToolOptions { recorder: Some(recorder.clone()), ..Default::default() },
        ..Default::default()
    };
    let tool = create_tool_with_options(name, dir, &options).unwrap();
    let result = tool.execute("call-1", args, CancellationToken::new(), noop()).await.map(drop);
    let changes = std::mem::take(&mut *recorder.changes.lock().unwrap());
    (result, changes)
}

#[tokio::test]
async fn write_reports_created_and_overwritten_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("old.txt"), "before").unwrap();
    let (result, changes) = run(dir.path(), 1024, "write", json!({"path": "new.txt", "content": "fresh"})).await;
    result.unwrap();
    assert_eq!(changes, vec![(dir.path().join("new.txt"), Some(PriorContent::Missing), b"fresh".to_vec())]);
    let (_, changes) = run(dir.path(), 1024, "write", json!({"path": "old.txt", "content": "after"})).await;
    assert_eq!(changes[0].1, Some(PriorContent::Bytes(b"before".to_vec())));
}

#[tokio::test]
async fn edit_reports_only_changes_it_writes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "alpha\n").unwrap();
    let edit = |old: &str| json!({"path": "f.txt", "edits": [{"oldText": old, "newText": "ALPHA"}]});
    let (result, changes) = run(dir.path(), 1024, "edit", edit("missing")).await;
    assert!(result.is_err());
    assert!(changes.is_empty(), "a failed edit records nothing");
    let (result, changes) = run(dir.path(), 1024, "edit", edit("alpha")).await;
    result.unwrap();
    assert_eq!(
        changes,
        vec![(dir.path().join("f.txt"), Some(PriorContent::Bytes(b"alpha\n".to_vec())), b"ALPHA\n".to_vec())]
    );
}

#[tokio::test]
async fn files_above_the_cap_are_not_kept() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.txt"), "0123456789").unwrap();
    let (_, changes) = run(dir.path(), 4, "write", json!({"path": "big.txt", "content": "small"})).await;
    assert_eq!(changes[0].1, Some(PriorContent::NotRestorable(too_large_reason(4))));
}
