//! A socket-file lock owner in another process (upstream `processLock` in `manager.ts`). Its own binary: the
//! child it spawns can inherit a socket another test creates at the same time (macOS sets close-on-exec late).
#![cfg(all(unix, not(target_os = "solaris")))]

use std::path::Path;

use pi_chord::host2::ChordErrorCode;
use pi_chord::host2::process_lock::socket_file_lock;

fn assert_locked(error: &pi_chord::ChordError, name: &str) {
    assert_eq!((error.code(), error.reason()), (Some(ChordErrorCode::Unavailable), Some("locked")), "{error}");
    assert_eq!(error.to_string(), format!("unavailable/locked: lock {name} is held by another process"));
}

// macOS temp dirs are deep enough to push a socket path past its 104-byte limit.
fn short_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new().tempdir_in("/tmp").unwrap()
}

fn socket_path(dir: &Path, name: &str) -> std::path::PathBuf {
    let digest = ring::digest::digest(&ring::digest::SHA256, name.as_bytes());
    let key: String = digest.as_ref()[..16].iter().map(|byte| format!("{byte:02x}")).collect();
    dir.join(format!("chord2-{key}.sock"))
}

#[tokio::test]
async fn an_owner_in_another_process_protects_a_socket_not_yet_bound() {
    use std::os::unix::fs::OpenOptionsExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use rustix::fs::{FlockOperation, flock};

    const CHILD_DIR: &str = "PI_CHORD_LOCK_OWNER_CHILD_DIR";
    let name = "owner-between-probe-and-bind";
    if let Some(dir) = std::env::var_os(CHILD_DIR) {
        let result = socket_file_lock(name, Path::new(&dir)).await;
        let Err(error) = result else { panic!("a second holder ignored kernel ownership") };
        assert_locked(&error, name);
        return;
    }
    let dir = short_tempdir();
    let path = socket_path(dir.path(), name).with_extension("owner");
    let owner = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(path).unwrap();
    flock(&owner, FlockOperation::NonBlockingLockExclusive).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "an_owner_in_another_process_protects_a_socket_not_yet_bound", "--nocapture"])
        .env(CHILD_DIR, dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "contending child failed: {status}");
            break;
        }
        if started.elapsed() > Duration::from_secs(10) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("contending child did not exit");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(owner);
    socket_file_lock(name, dir.path()).await.unwrap().release();
}
