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
/// Retries of a Windows delete-pending denial, capped apart from the wait for another holder so a
/// real permission error fails fast.
const MAX_PENDING_DELETE_RETRIES: u32 = 5;

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

/// Windows denies access to create a directory another holder removed while a handle to it is
/// still open, so that error means the lock is busy, like `AlreadyExists`.
fn is_pending_delete(error: &std::io::Error) -> bool {
    cfg!(windows) && error.kind() == ErrorKind::PermissionDenied
}

fn touch(dir: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.set_modified(SystemTime::now());
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// The result of one attempt to take the lock.
enum Attempt {
    Acquired(FileLock),
    /// Another holder has a fresh lock.
    Held,
    /// Windows denied access to a lock directory that is delete-pending.
    PendingDelete(std::io::Error),
}

impl FileLock {
    /// Take the lock once. `Ok(None)` means another holder has a fresh lock.
    pub fn try_acquire(file: &Path, stale: Duration) -> std::io::Result<Option<Self>> {
        Ok(match Self::attempt(file, stale)? {
            Attempt::Acquired(lock) => Some(lock),
            Attempt::Held | Attempt::PendingDelete(_) => None,
        })
    }

    fn attempt(file: &Path, stale: Duration) -> std::io::Result<Attempt> {
        let dir = lock_dir(file);
        if let Some(parent) = dir.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::create_dir(&dir) {
            Ok(()) => Ok(Attempt::Acquired(Self { dir, keepalive: None })),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if is_stale(&dir, stale) {
                    let _ = std::fs::remove_dir(&dir);
                    return match std::fs::create_dir(&dir) {
                        Ok(()) => Ok(Attempt::Acquired(Self { dir, keepalive: None })),
                        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(Attempt::Held),
                        Err(error) if is_pending_delete(&error) => Ok(Attempt::PendingDelete(error)),
                        Err(error) => Err(error),
                    };
                }
                Ok(Attempt::Held)
            }
            Err(error) if is_pending_delete(&error) => Ok(Attempt::PendingDelete(error)),
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
        Self::acquire_with(file, cancel, || Self::attempt(file, ASYNC_STALE)).await
    }

    async fn acquire_with(
        file: &Path,
        cancel: &CancellationToken,
        mut attempt: impl FnMut() -> std::io::Result<Attempt>,
    ) -> AuthResult<Self> {
        let deadline = tokio::time::Instant::now() + ASYNC_STALE;
        let (mut retry, mut pending_deletes) = (0u32, 0u32);
        loop {
            if cancel.is_cancelled() {
                return Err(AuthError::Cancelled);
            }
            let storage_error =
                |error: std::io::Error| AuthError::Storage(format!("cannot lock {}: {error}", file.display()));
            match attempt() {
                Ok(Attempt::Acquired(mut lock)) => {
                    lock.start_keepalive();
                    return Ok(lock);
                }
                Ok(Attempt::Held) => {}
                Ok(Attempt::PendingDelete(error)) => {
                    pending_deletes += 1;
                    if pending_deletes > MAX_PENDING_DELETE_RETRIES {
                        return Err(storage_error(error));
                    }
                }
                Err(error) => return Err(storage_error(error)),
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

    fn denied() -> std::io::Result<Attempt> {
        Ok(Attempt::PendingDelete(std::io::Error::from(ErrorKind::PermissionDenied)))
    }

    #[tokio::test(start_paused = true)]
    async fn a_persistent_delete_pending_denial_fails_after_a_bounded_number_of_retries() {
        let file = std::env::temp_dir().join("pi-lock-pending/auth.json");
        let calls = std::cell::Cell::new(0);
        let attempt = || {
            calls.set(calls.get() + 1);
            denied()
        };
        let started = tokio::time::Instant::now();
        let result = FileLock::acquire_with(&file, &CancellationToken::new(), attempt).await;
        let Err(AuthError::Storage(message)) = result else { panic!("expected a storage error") };
        assert!(message.starts_with("cannot lock ") && message.contains("permission denied"), "{message}");
        assert_eq!(calls.get(), 6);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn a_transient_delete_pending_denial_is_retried() {
        let dir = std::env::temp_dir().join(format!("pi-lock-transient-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("auth.json");
        let calls = std::cell::Cell::new(0);
        let attempt = || {
            calls.set(calls.get() + 1);
            if calls.get() == 1 { denied() } else { FileLock::attempt(&file, ASYNC_STALE) }
        };
        let lock = FileLock::acquire_with(&file, &CancellationToken::new(), attempt).await.unwrap();
        assert_eq!(calls.get(), 2);
        drop(lock);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_lock_keeps_waiting_for_the_stale_period() {
        let file = std::env::temp_dir().join("pi-lock-held/auth.json");
        let calls = std::cell::Cell::new(0);
        let attempt = || {
            calls.set(calls.get() + 1);
            Ok(Attempt::Held)
        };
        let started = tokio::time::Instant::now();
        let result = FileLock::acquire_with(&file, &CancellationToken::new(), attempt).await;
        assert!(matches!(result, Err(AuthError::Storage(message)) if message.contains("locked by another process")));
        assert!(calls.get() > 6);
        assert!(started.elapsed() >= ASYNC_STALE);
    }
}
