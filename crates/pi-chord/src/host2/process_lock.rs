//! Port of upstream `processLock` (`manager.ts`): one live process per lock name. Linux and Android bind an
//! abstract socket and Windows creates a named pipe; the kernel drops either with its process, so nothing stale
//! is left. Other Unix systems bind a socket file, removed on release; a leftover file nobody accepts on is taken over.

use std::io;
use std::sync::Mutex;

use crate::error::{ChordError, ChordResult};
use crate::host2::errors::ChordErrorCode;
use crate::sync::lock;

#[cfg(unix)]
mod owner;

#[cfg(unix)]
pub use file::socket_file_lock;

/// A held lock. Dropping it (or [`ProcessLock::release`]) releases it; so does the end of the process.
pub struct ProcessLock {
    _held: Held,
}

/// What holds the lock; each is kept only to be dropped.
enum Held {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    Abstract { _listener: std::os::unix::net::UnixListener },
    #[cfg(unix)]
    File { _socket: file::FileLock },
    #[cfg(windows)]
    Pipe { _server: tokio::net::windows::named_pipe::NamedPipeServer },
}

impl ProcessLock {
    pub fn release(self) {
        drop(self);
    }
}

/// Acquires the kernel lock `name`, or fails with `unavailable/locked` while another process holds it.
pub async fn process_lock(name: &str) -> ChordResult<ProcessLock> {
    platform_lock(name, &lock_key(name)).await
}

/// Keeps `lock` until the process exits (a child's lock follows the process that owns the data).
pub fn hold_for_process(lock_held: ProcessLock) {
    static HELD: Mutex<Vec<ProcessLock>> = Mutex::new(Vec::new());
    lock(&HELD).push(lock_held);
}

/// The first 32 hex digits of the name's SHA-256, as upstream names its sockets and pipes.
fn lock_key(name: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, name.as_bytes());
    digest.as_ref()[..16].iter().map(|byte| format!("{byte:02x}")).collect()
}

fn locked(name: &str) -> ChordError {
    ChordError::with_reason(ChordErrorCode::Unavailable, "locked", format!("lock {name} is held by another process"))
}

fn io_failure(error: io::Error) -> ChordError {
    ChordError::other(error)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
async fn platform_lock(name: &str, key: &str) -> ChordResult<ProcessLock> {
    #[cfg(target_os = "android")]
    use std::os::android::net::SocketAddrExt;
    #[cfg(target_os = "linux")]
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};

    let address = SocketAddr::from_abstract_name(format!("chord2-{key}")).map_err(io_failure)?;
    match UnixListener::bind_addr(&address) {
        Ok(listener) => Ok(ProcessLock { _held: Held::Abstract { _listener: listener } }),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => Err(locked(name)),
        Err(error) => Err(io_failure(error)),
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
async fn platform_lock(name: &str, _key: &str) -> ChordResult<ProcessLock> {
    socket_file_lock(name, &std::env::temp_dir()).await
}

#[cfg(windows)]
async fn platform_lock(name: &str, key: &str) -> ChordResult<ProcessLock> {
    use tokio::net::windows::named_pipe::ServerOptions;

    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_PIPE_BUSY: i32 = 231;
    match ServerOptions::new().first_pipe_instance(true).create(format!(r"\\.\pipe\chord2-{key}")) {
        Ok(server) => Ok(ProcessLock { _held: Held::Pipe { _server: server } }),
        Err(error) if matches!(error.raw_os_error(), Some(ERROR_ACCESS_DENIED | ERROR_PIPE_BUSY)) => Err(locked(name)),
        Err(error) => Err(io_failure(error)),
    }
}

#[cfg(not(any(unix, windows)))]
async fn platform_lock(_name: &str, _key: &str) -> ChordResult<ProcessLock> {
    Err(ChordError::code_only(ChordErrorCode::Unavailable, "process locks are not supported on this platform"))
}

#[cfg(unix)]
mod file {
    use std::fs::{self, DirBuilder};
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use super::{Held, ProcessLock, io_failure, lock_key, locked};
    use crate::error::{ChordError, ChordResult};
    use crate::host2::errors::ChordErrorCode;

    /// `sun_path` holds 104 bytes on macOS and the BSDs, including the terminating NUL.
    const MAX_SOCKET_PATH_BYTES: usize = 103;

    /// A lock held by a socket file in `dir`: what [`super::process_lock`] uses, in the temporary directory, where
    /// abstract sockets are missing.
    pub async fn socket_file_lock(name: &str, dir: &Path) -> ChordResult<ProcessLock> {
        let path = lock_socket_path(dir, &format!("chord2-{}.sock", lock_key(name)))?;
        let owner = super::owner::claim(name, &path)?;
        let listener = loop {
            match UnixListener::bind(&path) {
                Ok(listener) => break listener,
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                    if accepting(&path) {
                        return Err(locked(name));
                    }
                    match fs::remove_file(&path) {
                        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(io_failure(error)),
                        _ => {}
                    }
                }
                Err(error) => return Err(io_failure(error)),
            }
        };
        let held = FileLock::serve(listener, path, owner).map_err(io_failure)?;
        Ok(ProcessLock { _held: Held::File { _socket: held } })
    }

    /// The socket's path in `dir`, or, when that does not fit in `sun_path` (a deep TMPDIR), in a private per-user
    /// directory under /tmp. Other users must not create or remove sockets there, so it must be ours and closed.
    fn lock_socket_path(dir: &Path, file: &str) -> ChordResult<PathBuf> {
        let preferred = dir.join(file);
        if preferred.as_os_str().len() <= MAX_SOCKET_PATH_BYTES {
            return Ok(preferred);
        }
        let uid = rustix::process::getuid().as_raw();
        let directory = PathBuf::from(format!("/tmp/chord2-{uid}"));
        DirBuilder::new().recursive(true).mode(0o700).create(&directory).map_err(io_failure)?;
        let stats = fs::symlink_metadata(&directory).map_err(io_failure)?;
        if !stats.is_dir() || stats.uid() != uid || stats.mode() & 0o077 != 0 {
            let message =
                format!("lock directory {} must be a directory only its owner can access", directory.display());
            return Err(ChordError::code_only(ChordErrorCode::Unavailable, message));
        }
        Ok(directory.join(file))
    }

    /// Whether a live process accepts on the socket (a refused or missing one is stale).
    fn accepting(path: &Path) -> bool {
        match UnixStream::connect(path) {
            Ok(_) => true,
            Err(error) => !matches!(error.kind(), io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound),
        }
    }

    /// A bound socket file and the thread that accepts (and drops) connections on it, so probes see it live.
    pub(super) struct FileLock {
        path: PathBuf,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
        _owner: Option<fs::File>,
    }

    impl FileLock {
        fn serve(listener: UnixListener, path: PathBuf, owner: Option<fs::File>) -> io::Result<Self> {
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let thread = std::thread::Builder::new().name("chord2-lock".into()).spawn(move || {
                for connection in listener.incoming() {
                    if flag.load(Ordering::SeqCst) {
                        return;
                    }
                    if connection.is_err() {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            })?;
            Ok(Self { path, stop, thread: Some(thread), _owner: owner })
        }
    }

    impl Drop for FileLock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // A connection wakes the accept loop. When the file is gone nobody can reach the socket: leave the thread.
            if let Some(thread) = self.thread.take() {
                if UnixStream::connect(&self.path).is_ok() {
                    let _ = thread.join();
                }
            }
            let _ = fs::remove_file(&self.path);
        }
    }
}
