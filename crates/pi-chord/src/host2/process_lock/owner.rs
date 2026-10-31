//! Kernel ownership for socket-file locks: a failed socket probe cannot evict a live holder.

use std::fs::File;
use std::path::Path;

use crate::ChordResult;

pub(super) fn claim(name: &str, socket: &Path) -> ChordResult<Option<File>> {
    #[cfg(target_os = "solaris")]
    {
        // rustix has no flock on Solaris; keep its existing socket-probe behavior.
        let _ = (name, socket);
        Ok(None)
    }
    #[cfg(not(target_os = "solaris"))]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

        use rustix::fs::{FlockOperation, OFlags, flock};

        use super::{io_failure, locked};

        // Keep this inode after release: unlinking it would let contenders lock different files.
        let path = socket.with_extension("owner");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(OFlags::NOFOLLOW.bits() as i32)
            .open(&path)
            .map_err(io_failure)?;
        let stats = file.metadata().map_err(io_failure)?;
        if !stats.is_file() || stats.uid() != rustix::process::getuid().as_raw() || stats.mode() & 0o077 != 0 {
            return Err(io_failure(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("lock file {} must be a regular file only its owner can access", path.display()),
            )));
        }
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(file)),
            Err(error) if error == rustix::io::Errno::WOULDBLOCK => Err(locked(name)),
            Err(error) => Err(io_failure(error.into())),
        }
    }
}
