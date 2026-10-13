//! File helpers for the file-backed host adapters: hashing, atomic writes, process-local locks.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use ring::digest::{SHA256, digest};

static LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Default::default);

/// Serialize operations sharing `key` within this process, across adapter instances.
pub(crate) async fn serialized<T>(key: &str, action: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    let lock = {
        let mut locks = LOCKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.entry(key.to_string()).or_default().clone()
    };
    let _guard = lock.lock().await;
    action.await
}

pub(crate) fn sha256_hex(value: &str) -> String {
    digest(&SHA256, value.as_bytes()).as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Read a UTF-8 file, or `None` when it does not exist.
pub(crate) async fn read_optional(path: &Path) -> anyhow::Result<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Create `path` as a private directory (0700 on Unix) if missing.
pub(crate) async fn ensure_private_dir(path: &Path) -> anyhow::Result<()> {
    let mut builder = tokio::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path).await?;
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    let unique =
        format!("{}.{}", std::process::id(), sha256_hex(&format!("{:?}{:?}", std::time::SystemTime::now(), path)));
    let mut name = path.file_name().map(|name| name.to_os_string()).unwrap_or_default();
    name.push(format!(".{}.tmp", &unique[..24]));
    path.with_file_name(name)
}

/// Write through a synced temporary file and rename it over `path`; `mode` applies on Unix.
pub(crate) async fn atomic_write(path: &Path, text: &str, mode: u32) -> anyhow::Result<()> {
    let (path, text) = (path.to_path_buf(), text.to_string());
    tokio::task::spawn_blocking(move || write_blocking(&path, &text, mode)).await?
}

fn write_blocking(path: &Path, text: &str, mode: u32) -> anyhow::Result<()> {
    let temporary = temporary_path(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
    #[cfg(not(unix))]
    let _ = mode;
    let written = options.open(&temporary).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.sync_all()
    });
    if let Err(error) = written.and_then(|()| std::fs::rename(&temporary, path)) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// The permission bits of `path` (0o600 where modes do not apply).
pub(crate) fn file_mode(metadata: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o777
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0o600
    }
}
