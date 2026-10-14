//! A port of the `proper-lockfile` behavior upstream uses: an atomic `<path>.lock` directory whose
//! mtime is refreshed while held and that other processes may take over once it is stale.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use pi_chord::{ChordError, ChordResult};
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug)]
pub struct LockOptions {
    /// Resolve symlinks of the locked path first (the path must exist).
    pub realpath: bool,
    pub stale: Duration,
    pub update: Duration,
    pub retry_interval: Duration,
    pub max_retry_time: Duration,
}

/// A held lock. `release` removes it; dropping it only stops the refresh, so the lock goes stale.
pub struct LockRelease {
    lock_path: PathBuf,
    stop: Arc<Notify>,
    released: bool,
}

impl LockRelease {
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub async fn release(mut self) -> ChordResult<()> {
        self.released = true;
        self.stop.notify_one();
        match tokio::fs::remove_dir(&self.lock_path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(ChordError::error("Lock is already released"))
            }
            Err(error) => Err(ChordError::other(error)),
        }
    }
}

impl Drop for LockRelease {
    fn drop(&mut self) {
        if !self.released {
            self.stop.notify_one();
        }
    }
}

fn is_stale(path: &Path, stale: Duration) -> bool {
    let modified = std::fs::metadata(path).and_then(|metadata| metadata.modified());
    match modified {
        Ok(modified) => SystemTime::now().duration_since(modified).is_ok_and(|age| age > stale),
        Err(_) => false,
    }
}

/// Take `<path>.lock`, retrying until `max_retry_time` and taking over stale locks.
pub async fn lock(path: &Path, options: LockOptions) -> ChordResult<LockRelease> {
    let path = if options.realpath {
        tokio::fs::canonicalize(path).await.map_err(ChordError::other)?
    } else {
        path.to_path_buf()
    };
    let mut lock_path = path.into_os_string();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    let deadline = tokio::time::Instant::now() + options.max_retry_time;
    loop {
        match tokio::fs::create_dir(&lock_path).await {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale(&lock_path, options.stale) {
                    let _ = tokio::fs::remove_dir(&lock_path).await;
                    continue;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(ChordError::error("Lock file is already being held"));
                }
                tokio::time::sleep(options.retry_interval).await;
            }
            Err(error) => return Err(ChordError::other(error)),
        }
    }
    let stop = Arc::new(Notify::new());
    let refresh_stop = stop.clone();
    let refresh_path = lock_path.clone();
    let update = options.update;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = refresh_stop.notified() => return,
                () = tokio::time::sleep(update) => {}
            }
            let path = refresh_path.clone();
            let touched = tokio::task::spawn_blocking(move || {
                std::fs::File::open(&path).and_then(|file| file.set_modified(SystemTime::now()))
            })
            .await;
            if !matches!(touched, Ok(Ok(()))) {
                return;
            }
        }
    });
    Ok(LockRelease { lock_path, stop, released: false })
}
