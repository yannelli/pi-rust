//! `<agentDir>/marketplace/.lock` (`<pid> <unix ms>`) serializes installs and pruning across
//! pi processes. A lock older than `STALE_AFTER` or whose process is gone is taken over.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};

static LOCK: Mutex<()> = Mutex::new(());

pub(crate) const LOCK_TIMEOUT: Duration = Duration::from_secs(15);
const STALE_AFTER: Duration = Duration::from_secs(180);
const MAX_BACKOFF: Duration = Duration::from_millis(250);

/// Held while installing or pruning; deletes the lock file on drop.
pub(crate) struct InstallLock {
    path: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(crate) fn lock_path(root: &Path) -> PathBuf {
    root.join(".lock")
}

fn unix_ms(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_millis()).unwrap_or_default()
}

/// Take the lock under `root`, waiting up to `timeout` for another holder.
pub(crate) fn acquire(root: &Path, timeout: Duration) -> anyhow::Result<InstallLock> {
    let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::fs::create_dir_all(root).with_context(|| format!("Could not create {}", root.display()))?;
    let path = lock_path(root);
    let deadline = Instant::now() + timeout;
    let mut delay = Duration::from_millis(10);
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let lock = InstallLock { path, _guard: guard };
                write!(file, "{} {}", std::process::id(), unix_ms(SystemTime::now()))
                    .with_context(|| format!("Could not write {}", lock.path.display()))?;
                return Ok(lock);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if remove_if_stale(&path) {
                    continue;
                }
                if Instant::now() >= deadline {
                    bail!(
                        "Another pi process is changing marketplace extensions ({} is held). Try again.",
                        path.display()
                    );
                }
                std::thread::sleep(delay);
                delay = (delay * 2).min(MAX_BACKOFF);
            }
            Err(error) => return Err(error).with_context(|| format!("Could not create {}", path.display())),
        }
    }
}

/// Remove the lock file when it is stale and unchanged since it was read.
fn remove_if_stale(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else { return false };
    let modified = std::fs::metadata(path).and_then(|meta| meta.modified()).ok();
    if !is_stale(&text, modified, SystemTime::now()) {
        return false;
    }
    if std::fs::read_to_string(path).ok().as_deref() != Some(text.as_str()) {
        return false;
    }
    std::fs::remove_file(path).is_ok()
}

fn is_stale(text: &str, modified: Option<SystemTime>, now: SystemTime) -> bool {
    let mut fields = text.split_whitespace();
    let pid = fields.next().and_then(|pid| pid.parse::<u32>().ok());
    let written = fields.next().and_then(|ms| ms.parse::<u128>().ok());
    let written = match (pid, written) {
        (Some(_), Some(written)) => written,
        // A holder that has created the file but not written it yet.
        _ => modified.map(unix_ms).unwrap_or_else(|| unix_ms(now)),
    };
    let old = unix_ms(now).saturating_sub(written) > STALE_AFTER.as_millis();
    old || pid.is_some_and(|pid| pid == std::process::id() || !process_exists(pid))
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw) else { return false };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        Err(error) => error != rustix::io::Errno::SRCH,
    }
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn foreign_live_pid() -> u32 {
        #[cfg(unix)]
        return std::os::unix::process::parent_id();
        #[cfg(not(unix))]
        return std::process::id().wrapping_add(1);
    }

    #[test]
    fn a_lock_file_from_a_live_process_waits_for_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(dir.path());
        let held = format!("{} {}", foreign_live_pid(), unix_ms(SystemTime::now()));
        std::fs::write(&path, &held).unwrap();
        let error = acquire(dir.path(), Duration::from_millis(50)).err().unwrap();
        assert!(error.to_string().contains("Another pi process"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), held);

        let releaser = {
            let path = path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                std::fs::remove_file(path).unwrap();
            })
        };
        let started = Instant::now();
        let lock = acquire(dir.path(), Duration::from_secs(5)).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(90));
        releaser.join().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(&format!("{} ", std::process::id())), "{text}");
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn two_holders_in_one_process_serialize() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let first = acquire(&root, LOCK_TIMEOUT).unwrap();
        let waiter = std::thread::spawn(move || {
            let started = Instant::now();
            let _second = acquire(&root, LOCK_TIMEOUT).unwrap();
            started.elapsed()
        });
        std::thread::sleep(Duration::from_millis(100));
        drop(first);
        assert!(waiter.join().unwrap() >= Duration::from_millis(90));
    }

    #[test]
    fn a_stale_lock_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(dir.path());
        let old = unix_ms(SystemTime::now() - STALE_AFTER - Duration::from_secs(1));
        std::fs::write(&path, format!("{} {old}", foreign_live_pid())).unwrap();
        drop(acquire(dir.path(), Duration::from_millis(50)).unwrap());

        std::fs::write(&path, format!("{} {}", std::process::id(), unix_ms(SystemTime::now()))).unwrap();
        drop(acquire(dir.path(), Duration::from_millis(50)).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn staleness_uses_age_pid_and_partial_writes() {
        let now = SystemTime::now();
        let fresh = unix_ms(now);
        let live = foreign_live_pid();
        assert!(!is_stale(&format!("{live} {fresh}"), None, now));
        assert!(is_stale(&format!("{live} {}", fresh - STALE_AFTER.as_millis() - 1), None, now));
        assert!(!is_stale("", Some(now), now));
        assert!(is_stale("", Some(now - STALE_AFTER - Duration::from_secs(1)), now));
        #[cfg(unix)]
        assert!(is_stale(&format!("{} {fresh}", i32::MAX), None, now));
    }
}
