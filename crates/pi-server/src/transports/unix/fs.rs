//! Socket-path filesystem safety: stale-socket removal and identity-checked cleanup.

use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use pi_chord::{ChordError, ChordResult};

use crate::uuid::random_hex;

const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// The device and inode of one filesystem entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    dev: u64,
    ino: u64,
}

pub(crate) fn io_error(error: std::io::Error) -> ChordError {
    ChordError::other(error)
}

/// The identity of a socket at `path`, or `None` when it is missing or not a socket.
pub(crate) fn socket_identity(path: &Path) -> ChordResult<Option<FileIdentity>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            Ok(Some(FileIdentity { dev: metadata.dev(), ino: metadata.ino() }))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}

pub(crate) fn remove_path(path: &Path) -> ChordResult<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(io_error(error)),
        _ => Ok(()),
    }
}

fn sibling(path: &Path, prefix: &str) -> PathBuf {
    path.parent().unwrap_or(Path::new(".")).join(format!("{prefix}-{}", random_hex(6)))
}

/// The private path a listener binds before publishing it with a hard link.
pub(crate) fn owned_bind_path(path: &Path) -> PathBuf {
    let digest = ring::digest::digest(&ring::digest::SHA256, path.as_os_str().as_encoded_bytes());
    let suffix: String = digest.as_ref().iter().take(4).map(|byte| format!("{byte:02x}")).collect();
    path.parent().unwrap_or(Path::new(".")).join(format!("bind-{suffix}"))
}

async fn is_socket_live(path: &Path) -> ChordResult<bool> {
    match tokio::time::timeout(SOCKET_PROBE_TIMEOUT, tokio::net::UnixStream::connect(path)).await {
        Err(_) => Ok(true),
        Ok(Ok(_)) => Ok(true),
        Ok(Err(error)) => match error.kind() {
            ErrorKind::ConnectionRefused | ErrorKind::NotFound | ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {
                Ok(false)
            }
            _ => Err(io_error(error)),
        },
    }
}

/// Remove a socket nobody listens on. Never removes other file types or a live socket.
pub(crate) async fn remove_stale_socket(path: &Path) -> ChordResult<()> {
    let original = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(error)),
    };
    if !original.file_type().is_socket() {
        return Err(ChordError::error(format!("Refusing to remove non-socket Unix listener path: {}", path.display())));
    }
    if is_socket_live(path).await? {
        return Err(ChordError::error(format!("Unix listener is already running: {}", path.display())));
    }
    let preserved = sibling(path, "stale");
    match std::fs::rename(path, &preserved) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(error)),
        Ok(()) => {}
    }
    let expected = FileIdentity { dev: original.dev(), ino: original.ino() };
    if socket_identity(&preserved)? != Some(expected) {
        restore_if_vacant(&preserved, path)?;
        return Err(ChordError::error(format!(
            "Unix listener path changed while checking for a stale socket: {}",
            path.display()
        )));
    }
    remove_path(&preserved)
}

fn restore_if_vacant(preserved: &Path, path: &Path) -> ChordResult<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => std::fs::rename(preserved, path).map_err(io_error),
        Err(error) => Err(io_error(error)),
        Ok(_) => Ok(()),
    }
}

/// Remove the published socket only while it is still the inode this listener bound.
pub(crate) fn cleanup_owned_socket(path: &Path, identity: FileIdentity) -> ChordResult<()> {
    if socket_identity(path)? != Some(identity) {
        return Ok(());
    }
    let preserved = sibling(path, "cleanup");
    match std::fs::rename(path, &preserved) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(error)),
        Ok(()) => {}
    }
    if socket_identity(&preserved)? == Some(identity) {
        return remove_path(&preserved);
    }
    restore_if_vacant(&preserved, path)?;
    Err(ChordError::error(format!(
        "Unix listener path changed during cleanup; preserved replacement at {}",
        preserved.display()
    )))
}
