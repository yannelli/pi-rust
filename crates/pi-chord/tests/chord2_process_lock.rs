//! Process locks (upstream `processLock` in `manager.ts`): one holder per name until it releases or exits. The
//! socket-file variant (macOS and other Unix systems) runs here too, including upstream's "admits one holder
//! when the temporary directory is too deep for a socket path".

use pi_chord::host2::ChordErrorCode;
use pi_chord::host2::process_lock::process_lock;

fn unique(name: &str) -> String {
    format!("chord2-test-{}-{name}", std::process::id())
}

fn assert_locked(error: &pi_chord::ChordError, name: &str) {
    assert_eq!((error.code(), error.reason()), (Some(ChordErrorCode::Unavailable), Some("locked")), "{error}");
    assert_eq!(error.to_string(), format!("unavailable/locked: lock {name} is held by another process"));
}

#[tokio::test]
async fn admits_one_holder_per_name_until_it_releases() {
    let name = unique("one");
    let held = process_lock(&name).await.unwrap();
    let Err(error) = process_lock(&name).await else { panic!("a second holder") };
    assert_locked(&error, &name);
    let other = process_lock(&unique("other")).await.unwrap();
    held.release();
    let again = process_lock(&name).await.unwrap();
    drop(again);
    process_lock(&name).await.unwrap().release();
    other.release();
}

#[cfg(unix)]
mod socket_file {
    use std::os::unix::net::UnixListener;
    use std::path::Path;

    use pi_chord::host2::process_lock::socket_file_lock;

    use super::{assert_locked, unique};

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
    async fn admits_one_holder_and_removes_its_file_on_release() {
        let dir = short_tempdir();
        let name = unique("file");
        let held = socket_file_lock(&name, dir.path()).await.unwrap();
        let path = socket_path(dir.path(), &name);
        assert!(path.exists());
        // More probes than a listen backlog holds: the holder keeps accepting them.
        for _ in 0..200 {
            let Err(error) = socket_file_lock(&name, dir.path()).await else { panic!("a second holder") };
            assert_locked(&error, &name);
        }
        held.release();
        assert!(!path.exists());
        socket_file_lock(&name, dir.path()).await.unwrap().release();
    }

    // A socket path over 104 bytes failed with EINVAL on macOS when TMPDIR was deep.
    #[tokio::test]
    async fn admits_one_holder_when_the_directory_is_too_deep_for_a_socket_path() {
        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("a".repeat(60)).join("b".repeat(60));
        std::fs::create_dir_all(&deep).unwrap();
        let name = unique("deep");
        let held = socket_file_lock(&name, &deep).await.unwrap();
        let uid = rustix::process::getuid().as_raw();
        assert!(socket_path(Path::new(&format!("/tmp/chord2-{uid}")), &name).exists());
        let Err(error) = socket_file_lock(&name, &deep).await else { panic!("a second holder") };
        assert_locked(&error, &name);
        held.release();
        socket_file_lock(&name, &deep).await.unwrap().release();
    }

    #[tokio::test]
    async fn takes_over_a_leftover_file_nobody_accepts_on() {
        let dir = short_tempdir();
        let name = unique("stale");
        drop(UnixListener::bind(socket_path(dir.path(), &name)).unwrap());
        assert!(socket_path(dir.path(), &name).exists());
        let held = socket_file_lock(&name, dir.path()).await.unwrap();
        let Err(error) = socket_file_lock(&name, dir.path()).await else { panic!("a second holder") };
        assert_locked(&error, &name);
        held.release();
    }

    #[cfg(not(target_os = "solaris"))]
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
            .args([
                "--exact",
                "socket_file::an_owner_in_another_process_protects_a_socket_not_yet_bound",
                "--nocapture",
            ])
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
}
