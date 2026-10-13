//! Directory locks compatible with `proper-lockfile`: `<path>.lock` is created with `mkdir`, its
//! mtime is renewed while held, and a lock whose mtime is older than `stale` is taken over.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio_util::sync::CancellationToken;

pub(crate) struct LockOptions {
    pub stale: Duration,
    pub retries: u32,
    pub retry_delay: Duration,
}

pub(crate) struct DirLock {
    path: PathBuf,
    renew: Option<CancellationToken>,
}

impl Drop for DirLock {
    fn drop(&mut self) {
        if let Some(renew) = &self.renew {
            renew.cancel();
        }
        let _ = std::fs::remove_dir(&self.path);
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    PathBuf::from(lock)
}

#[cfg(unix)]
fn touch(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.set_modified(SystemTime::now())
}

#[cfg(windows)]
fn touch(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    file.set_modified(SystemTime::now())
}

#[cfg(not(any(unix, windows)))]
fn touch(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// One try: create the lock, or take over a stale one. `Ok(false)` while another holder has it.
fn try_acquire(lock: &Path, stale: Duration) -> std::io::Result<bool> {
    match std::fs::create_dir(lock) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let modified = std::fs::metadata(lock).and_then(|metadata| metadata.modified());
            let is_stale = modified.is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age > stale));
            if is_stale && std::fs::remove_dir(lock).is_ok() {
                return Ok(std::fs::create_dir(lock).is_ok());
            }
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn locked_error(path: &Path) -> String {
    format!("Lock file is already being held: {}", path.display())
}

/// Lock `path` from synchronous code, waiting between tries.
pub(crate) fn lock_sync(path: &Path, options: &LockOptions) -> Result<DirLock, String> {
    let lock = lock_path(path);
    for attempt in 0..=options.retries {
        if try_acquire(&lock, options.stale).map_err(|error| error.to_string())? {
            return Ok(DirLock { path: lock, renew: None });
        }
        if attempt < options.retries {
            std::thread::sleep(options.retry_delay);
        }
    }
    Err(locked_error(path))
}

/// Lock `path`, renewing the lock in the background while it is held.
pub(crate) async fn lock_async(path: &Path, options: &LockOptions) -> Result<DirLock, String> {
    let lock = lock_path(path);
    for attempt in 0..=options.retries {
        if try_acquire(&lock, options.stale).map_err(|error| error.to_string())? {
            let renew = CancellationToken::new();
            let (stop, renewed, interval) = (renew.clone(), lock.clone(), options.stale / 2);
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(interval) => { let _ = touch(&renewed); }
                        _ = stop.cancelled() => return,
                    }
                }
            });
            return Ok(DirLock { path: lock, renew: Some(renew) });
        }
        if attempt < options.retries {
            tokio::time::sleep(options.retry_delay).await;
        }
    }
    Err(locked_error(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_over_stale_locks_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state");
        let options = LockOptions { stale: Duration::from_secs(10), retries: 1, retry_delay: Duration::from_millis(1) };
        let held = lock_sync(&file, &options).unwrap();
        assert!(lock_sync(&file, &options).is_err());
        drop(held);
        assert!(!dir.path().join("state.lock").exists());
        let _held = lock_sync(&file, &options).unwrap();
        let fresh = LockOptions { stale: Duration::ZERO, ..options };
        std::thread::sleep(Duration::from_millis(5));
        assert!(lock_sync(&file, &fresh).is_ok());
    }
}
