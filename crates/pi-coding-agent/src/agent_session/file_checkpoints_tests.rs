use std::path::Path;

use super::*;

const SESSION: &str = "session-a";

/// Records a tool write of `content` to `path`, reading the prior state when it is wanted.
fn write(store: &FileCheckpoints, path: &Path, content: &str) {
    let prior = store.wants_prior(SESSION, path).then(|| match std::fs::read(path) {
        Ok(bytes) => PriorContent::Bytes(bytes),
        Err(_) => PriorContent::Missing,
    });
    std::fs::write(path, content).unwrap();
    store.record(SESSION, path, prior, content.as_bytes());
}

#[test]
fn records_the_prior_state_once_per_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "original").unwrap();
    let store = FileCheckpoints::default();
    store.begin_prompt(SESSION);
    assert!(store.wants_prior(SESSION, &file));
    write(&store, &file, "first");
    assert!(!store.wants_prior(SESSION, &file), "the second change of a prompt keeps the first prior state");
    assert!(!store.wants_prior(SESSION, &dir.path().join(".").join("a.txt")), "spellings share a checkpoint");
    write(&store, &file, "second");
    assert_eq!(store.prompt_count(SESSION), 1);

    let report = store.undo(SESSION).unwrap();
    assert_eq!(report.restored, vec![file.clone()]);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
    assert!(store.undo(SESSION).is_none());
}

#[test]
fn deletes_created_files_and_restores_modified_ones() {
    let dir = tempfile::tempdir().unwrap();
    let (created, modified) = (dir.path().join("new.txt"), dir.path().join("old.txt"));
    std::fs::write(&modified, "before").unwrap();
    let store = FileCheckpoints::default();
    store.begin_prompt(SESSION);
    write(&store, &created, "fresh");
    write(&store, &modified, "after");

    let report = store.undo(SESSION).unwrap();
    assert_eq!(report.deleted, vec![created.clone()]);
    assert_eq!(report.restored, vec![modified.clone()]);
    assert!(report.skipped.is_empty());
    assert!(!created.exists());
    assert_eq!(std::fs::read_to_string(&modified).unwrap(), "before");
}

#[test]
fn keeps_no_bytes_of_files_above_the_size_cap() {
    let dir = tempfile::tempdir().unwrap();
    let (big, small) = (dir.path().join("big.bin"), dir.path().join("small.txt"));
    std::fs::write(&big, "0123456789abcdef").unwrap();
    std::fs::write(&small, "tiny").unwrap();
    let store = FileCheckpoints::new(MAX_UNDO_PROMPTS, 8);
    store.begin_prompt(SESSION);
    write(&store, &big, "replaced");
    write(&store, &small, "changed");

    let report = store.undo(SESSION).unwrap();
    assert_eq!(report.restored, vec![small]);
    assert_eq!(report.skipped, vec![(big.clone(), UndoSkip::NotRestorable(too_large_reason(8)))]);
    assert_eq!(std::fs::read_to_string(&big).unwrap(), "replaced");
}

#[test]
fn skips_files_changed_since_the_agent_wrote_them() {
    let dir = tempfile::tempdir().unwrap();
    let (edited, removed) = (dir.path().join("edited.txt"), dir.path().join("removed.txt"));
    std::fs::write(&edited, "v1").unwrap();
    std::fs::write(&removed, "v1").unwrap();
    let store = FileCheckpoints::default();
    store.begin_prompt(SESSION);
    write(&store, &edited, "agent");
    write(&store, &removed, "agent");
    std::fs::write(&edited, "user").unwrap();
    std::fs::remove_file(&removed).unwrap();

    let report = store.undo(SESSION).unwrap();
    assert!(report.restored.is_empty() && report.deleted.is_empty());
    assert_eq!(report.skipped, vec![(edited.clone(), UndoSkip::ChangedSince), (removed, UndoSkip::ChangedSince)]);
    assert_eq!(std::fs::read_to_string(&edited).unwrap(), "user");
}

#[test]
fn undoes_prompts_newest_first_and_caps_their_number() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "v0").unwrap();
    let store = FileCheckpoints::new(2, MAX_CHECKPOINT_FILE_BYTES);
    for version in ["v1", "v2", "v3"] {
        store.begin_prompt(SESSION);
        write(&store, &file, version);
    }
    store.begin_prompt(SESSION);
    assert_eq!(store.prompt_count(SESSION), 2, "a prompt without changes adds no group");

    assert_eq!(store.undo(SESSION).unwrap().remaining, 1);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");
    assert_eq!(store.undo(SESSION).unwrap().remaining, 0);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");
    assert!(store.undo(SESSION).is_none(), "the oldest prompt fell off the cap");
}

#[test]
fn drops_checkpoints_of_another_session() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    let store = FileCheckpoints::default();
    store.begin_prompt(SESSION);
    write(&store, &file, "agent");
    assert!(store.undo("session-b").is_none());
    assert!(store.undo(SESSION).is_none());
    assert!(file.exists());
}

#[test]
fn a_change_without_a_prior_state_is_not_restorable() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "agent").unwrap();
    let store = FileCheckpoints::default();
    store.record(SESSION, &file, None, b"agent");
    let report = store.undo(SESSION).unwrap();
    assert!(matches!(&report.skipped[..], [(_, UndoSkip::NotRestorable(_))]), "{report:?}");
}

#[test]
fn restores_deleted_files_only_while_they_stay_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let (gone, recreated) = (dir.path().join("sub").join("gone.txt"), dir.path().join("recreated.txt"));
    let store = FileCheckpoints::default();
    store.begin_prompt(SESSION);
    store.record_change(SESSION, &gone, Some(PriorContent::Bytes(b"kept".to_vec())), Written::Deleted, false);
    store.record_change(SESSION, &recreated, Some(PriorContent::Bytes(b"before".to_vec())), Written::Deleted, true);
    std::fs::write(&recreated, "made by someone else").unwrap();

    let report = store.undo(SESSION).unwrap();
    assert_eq!(report.restored, vec![gone.clone()]);
    assert_eq!(std::fs::read_to_string(&gone).unwrap(), "kept");
    assert_eq!(report.skipped, vec![(recreated.clone(), UndoSkip::ChangedSince)]);
    assert_eq!(std::fs::read_to_string(&recreated).unwrap(), "made by someone else");
}

#[test]
fn unreported_changes_are_listed_and_can_stay() {
    let dir = tempfile::tempdir().unwrap();
    let (reported, detected) = (dir.path().join("reported.txt"), dir.path().join("detected.txt"));
    std::fs::write(&reported, "agent").unwrap();
    std::fs::write(&detected, "shell").unwrap();
    let store = FileCheckpoints::default();
    store.begin_prompt(SESSION);
    store.record_change(SESSION, &reported, Some(PriorContent::Missing), Written::Bytes(b"agent"), true);
    store.record_change(SESSION, &reported, Some(PriorContent::Missing), Written::OnDisk, false);
    store.record_change(SESSION, &detected, Some(PriorContent::Bytes(b"old".to_vec())), Written::OnDisk, true);
    let candidates = store.candidates(SESSION);
    let flags: Vec<bool> = candidates.iter().map(|candidate| candidate.detected).collect();
    assert_eq!(flags, [false, true], "a reported change clears the detected flag");

    let report = store.undo_with(SESSION, false).unwrap();
    assert_eq!(report.deleted, vec![reported.clone()]);
    assert_eq!(report.skipped, vec![(detected.clone(), UndoSkip::Unreported)]);
    assert_eq!(std::fs::read_to_string(&detected).unwrap(), "shell");
}

#[test]
fn file_fingerprints_match_the_bytes_in_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.bin");
    let bytes: Vec<u8> = (0..200_003u32).map(|n| (n % 251) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(Fingerprint::of_file(&path).unwrap(), Fingerprint::of(&bytes));
}
