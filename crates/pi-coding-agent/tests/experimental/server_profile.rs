//! Port of upstream `test/experimental-server-profile.test.ts`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pi_coding_agent::experimental::server::acquire_server_profile;

const FIRST_SERVER_ID: &str = "00000000-0000-4000-8000-000000000001";
const SECOND_SERVER_ID: &str = "00000000-0000-4000-8000-000000000002";

fn directory() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("psp-").tempdir().unwrap()
}

#[tokio::test]
async fn serializes_launchers_and_preserves_the_server_identity() {
    let directory = directory();
    let first = acquire_server_profile(directory.path(), None).await.unwrap();
    let acquired = Arc::new(AtomicBool::new(false));
    let (path, flag) = (directory.path().to_path_buf(), acquired.clone());
    let pending = tokio::spawn(async move {
        let profile = acquire_server_profile(&path, None).await.unwrap();
        flag.store(true, Ordering::SeqCst);
        profile
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!acquired.load(Ordering::SeqCst));
    let first_id = first.server_id.clone();
    first.release().await.unwrap();
    let second = pending.await.unwrap();
    assert!(acquired.load(Ordering::SeqCst));
    assert_eq!(second.server_id, first_id);
    let stored = std::fs::read_to_string(directory.path().join("default-server-id")).unwrap();
    assert_eq!(stored.trim(), first_id);
    second.release().await.unwrap();
}

#[tokio::test]
async fn does_not_serialize_different_server_ids_in_one_directory() {
    let directory = directory();
    let (first, second) = tokio::join!(
        acquire_server_profile(directory.path(), Some(FIRST_SERVER_ID)),
        acquire_server_profile(directory.path(), Some(SECOND_SERVER_ID)),
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(first.server_id, FIRST_SERVER_ID);
    assert_eq!(second.server_id, SECOND_SERVER_ID);
    let mut entries: Vec<String> = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    entries.sort();
    assert_eq!(entries, [format!("launcher-{FIRST_SERVER_ID}.lock"), format!("launcher-{SECOND_SERVER_ID}.lock")]);
    first.release().await.unwrap();
    second.release().await.unwrap();
}

#[tokio::test]
async fn does_not_share_the_default_identity_across_server_directories() {
    let (first_directory, second_directory) = (directory(), directory());
    let (first, second) = tokio::join!(
        acquire_server_profile(first_directory.path(), None),
        acquire_server_profile(second_directory.path(), None)
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_ne!(first.server_id, second.server_id);
    first.release().await.unwrap();
    second.release().await.unwrap();
}

#[tokio::test]
async fn rejects_a_corrupt_default_identity() {
    let directory = directory();
    std::fs::write(directory.path().join("default-server-id"), "invalid\n").unwrap();
    let error = acquire_server_profile(directory.path(), None).await.err().unwrap();
    assert!(error.to_string().contains("Invalid default experimental server identity"));
}

#[tokio::test]
async fn rejects_an_invalid_explicit_server_id() {
    let directory = directory();
    let error = acquire_server_profile(directory.path(), Some("invalid")).await.err().unwrap();
    assert!(error.to_string().contains("Invalid experimental server ID"));
}
