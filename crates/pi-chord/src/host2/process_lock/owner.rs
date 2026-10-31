//! Kernel ownership for socket-file locks: a failed socket probe cannot evict a live holder.

use std::fs::File;
use std::path::Path;

use crate::ChordResult;

#[cfg(target_os = "solaris")]
pub(super) type Owner = File;

#[cfg(not(target_os = "solaris"))]
pub(super) struct Owner(File);

#[cfg(not(target_os = "solaris"))]
impl std::os::fd::AsFd for Owner {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.0)
    }
}

#[cfg(not(target_os = "solaris"))]
impl Drop for Owner {
    fn drop(&mut self) {
        // Closing alone keeps ownership alive in descriptors inherited between fork and exec.
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }
}

pub(super) fn claim(name: &str, socket: &Path) -> ChordResult<Option<Owner>> {
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
            Ok(()) => Ok(Some(Owner(file))),
            Err(error) if error == rustix::io::Errno::WOULDBLOCK => Err(locked(name)),
            Err(error) => Err(io_failure(error.into())),
        }
    }
}

#[cfg(all(test, not(target_os = "solaris")))]
mod tests {
    use std::os::fd::AsFd;

    use super::claim;

    #[test]
    fn releasing_an_owner_unlocks_descriptors_inherited_by_another_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held.sock");
        let held = claim("inherited", &path).unwrap().unwrap();
        let inherited = held.as_fd().try_clone_to_owned().unwrap();
        drop(held);
        let next = claim("inherited", &path).expect("released ownership must not wait for a child to exec");
        drop(inherited);
        drop(next);
    }
}
