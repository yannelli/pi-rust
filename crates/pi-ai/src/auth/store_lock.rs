//! Cross-process file lock compatible with the `proper-lockfile` package the TypeScript pi
//! uses: the lock is a `<file>.lock` directory, and a lock whose directory has not been
//! touched for the stale period is taken over.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio_util::sync::CancellationToken;

use super::{AuthError, AuthResult};

/// Stale period for blocking locks (proper-lockfile default).
pub const SYNC_STALE: Duration = Duration::from_secs(10);
/// Stale period for async locks held across OAuth refreshes.
pub const ASYNC_STALE: Duration = Duration::from_secs(30);

/// A held lock. Dropping it removes the lock directory.
#[derive(Debug)]
pub struct FileLock {
    dir: PathBuf,
    keepalive: Option<CancellationToken>,
}

/// `<file>.lock`, the lock directory for `file`.
pub fn lock_dir(file: &Path) -> PathBuf {
    let mut name = file.file_name().map(|name| name.to_os_string()).unwrap_or_default();
    name.push(".lock");
    file.with_file_name(name)
}

fn is_stale(dir: &Path, stale: Duration) -> bool {
    let modified = std::fs::metadata(dir).and_then(|metadata| metadata.modified());
    match modified {
        Ok(modified) => SystemTime::now().duration_since(modified).is_ok_and(|age| age > stale),
        Err(error) => error.kind() == ErrorKind::NotFound,
    }
}

fn touch(dir: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.set_modified(SystemTime::now());
    }
    #[cfg(not(unix))]
    let _ = dir;
}

impl FileLock {
    /// Take the lock once. `Ok(None)` means another holder has a fresh lock.
    pub fn try_acquire(file: &Path, stale: Duration) -> std::io::Result<Option<Self>> {
        let dir = lock_dir(file);
        if let Some(parent) = dir.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::create_dir(&dir) {
            Ok(()) => Ok(Some(Self { dir, keepalive: None })),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if is_stale(&dir, stale) {
                    let _ = std::fs::remove_dir(&dir);
                    return match std::fs::create_dir(&dir) {
                        Ok(()) => Ok(Some(Self { dir, keepalive: None })),
                        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(None),
                        Err(error) => Err(error),
                    };
                }
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Blocking acquisition: up to 10 tries 20 ms apart, as upstream's `lockSync` loop.
    pub fn acquire_blocking(file: &Path) -> std::io::Result<Self> {
        for round in 1..=10 {
            if let Some(lock) = Self::try_acquire(file, SYNC_STALE)? {
                return Ok(lock);
            }
            if round < 10 {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Err(std::io::Error::new(ErrorKind::WouldBlock, format!("{} is locked", file.display())))
    }

    /// Async acquisition with exponential backoff until the stale period passes. While held,
    /// the lock directory is touched so other processes do not consider it stale.
    pub async fn acquire(file: &Path, cancel: &CancellationToken) -> AuthResult<Self> {
        let deadline = tokio::time::Instant::now() + ASYNC_STALE;
        let mut retry = 0u32;
        loop {
            if cancel.is_cancelled() {
                return Err(AuthError::Cancelled);
            }
            match Self::try_acquire(file, ASYNC_STALE) {
                Ok(Some(mut lock)) => {
                    lock.start_keepalive();
                    return Ok(lock);
                }
                Ok(None) => {}
                Err(error) => return Err(AuthError::Storage(format!("cannot lock {}: {error}", file.display()))),
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(AuthError::Storage(format!("{} is locked by another process", file.display())));
            }
            let base = (10u64 << retry.min(16)).min(1000);
            let jitter =
                SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
            let delay = Duration::from_millis(base + u64::from(jitter) % base.max(1)).min(remaining);
            retry += 1;
            tokio::select! {
                _ = cancel.cancelled() => return Err(AuthError::Cancelled),
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    fn start_keepalive(&mut self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let token = CancellationToken::new();
        let dir = self.dir.clone();
        let stop = token.clone();
        runtime.spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(ASYNC_STALE / 2) => touch(&dir),
                }
            }
        });
        self.keepalive = Some(token);
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if let Some(token) = &self.keepalive {
            token.cancel();
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_stale_locks_are_taken_over() {
        let dir = std::env::temp_dir().join(format!("pi-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("auth.json");
        assert_eq!(lock_dir(&file), dir.join("auth.json.lock"));
        let held = FileLock::try_acquire(&file, SYNC_STALE).unwrap().unwrap();
        assert!(FileLock::try_acquire(&file, SYNC_STALE).unwrap().is_none());
        std::thread::sleep(Duration::from_millis(5));
        let taken = FileLock::try_acquire(&file, Duration::from_millis(1)).unwrap();
        assert!(taken.is_some_and(|lock| lock.dir.exists()));
        drop(held);
        assert!(!lock_dir(&file).exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
