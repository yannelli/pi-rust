//! fs operations that need system calls std lacks: fchmod, fchown, lchmod (macOS), lutimes,
//! and statfs, through libc on Unix and windows-sys on Windows.
#![allow(unsafe_code)]

use std::fs::{self, File};
use std::io;
use std::path::Path;

use serde::Deserialize;

use super::fs::FileTable;
use super::fs_output::Output;
use crate::error::JsError;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub(crate) enum MetaRequest {
    Fchmod { fd: u64, mode: u32 },
    Fchown { fd: u64, uid: u32, gid: u32 },
    Lchmod { path: String, mode: u32 },
    Lutimes { path: String, atime: f64, mtime: f64 },
    Statfs { path: String },
}

pub(crate) fn run(request: MetaRequest, cwd: &Path, files: &FileTable) -> Result<Output, JsError> {
    let path_error = |syscall: &'static str, path: &str| {
        let path = path.to_string();
        move |error: io::Error| JsError::from_io(&error, syscall, Some(&path))
    };
    let fd_error = |syscall: &'static str| move |error: io::Error| JsError::from_io(&error, syscall, None);
    match request {
        MetaRequest::Fchmod { fd, mode } => {
            let file = files.get(fd, "fchmod")?;
            let file = file.lock().unwrap_or_else(|poison| poison.into_inner());
            fchmod(&file, mode).map_err(fd_error("fchmod"))?;
        }
        MetaRequest::Fchown { fd, uid, gid } => {
            let file = files.get(fd, "fchown")?;
            let file = file.lock().unwrap_or_else(|poison| poison.into_inner());
            fchown(&file, uid, gid).map_err(fd_error("fchown"))?;
        }
        MetaRequest::Lchmod { path, mode } => {
            lchmod(&cwd.join(&path), mode).map_err(path_error("lchmod", &path))?;
        }
        MetaRequest::Lutimes { path, atime, mtime } => {
            lutimes(&cwd.join(&path), atime, mtime).map_err(path_error("lutime", &path))?;
        }
        MetaRequest::Statfs { path } => {
            let fields = statfs(&cwd.join(&path)).map_err(path_error("statfs", &path))?;
            return Ok(Output::List(fields.iter().map(u64::to_string).collect()));
        }
    }
    Ok(Output::Undefined)
}

fn fchmod(file: &File, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let mut permissions = file.metadata()?.permissions();
        permissions.set_readonly(mode & 0o200 == 0);
        file.set_permissions(permissions)
    }
}

fn fchown(file: &File, uid: u32, gid: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::fchown(file, Some(uid), Some(gid))
    }
    #[cfg(not(unix))]
    {
        let _ = (file, uid, gid);
        Ok(())
    }
}

/// Node opens the link itself with `O_SYMLINK` and changes its mode; only macOS has that flag.
fn lchmod(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let file = fs::OpenOptions::new().write(true).custom_flags(libc::O_SYMLINK).open(path)?;
        fchmod(&file, mode)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (path, mode);
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[cfg(unix)]
fn lutimes(path: &Path, atime: f64, mtime: f64) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let spec = |seconds: f64| {
        let whole = seconds.floor();
        libc::timespec { tv_sec: whole as _, tv_nsec: ((seconds - whole) * 1e9) as _ }
    };
    let times = [spec(atime), spec(mtime)];
    // SAFETY: `path` is a valid C string and `times` holds the two entries utimensat reads.
    let result = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(windows)]
fn lutimes(path: &Path, atime: f64, mtime: f64) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    super::fs_ops::futimes(&file, atime, mtime)
}

/// libuv's `uv_statfs_t` fields: type, bsize, blocks, bfree, bavail, files, ffree.
#[cfg(unix)]
fn statfs(path: &Path) -> io::Result<[u64; 7]> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: an all-zero statfs is valid, and statfs only writes into it.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid C string and `buf` is a writable statfs.
    if unsafe { libc::statfs(path.as_ptr(), &mut buf) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast)]
    Ok([
        buf.f_type as u64,
        buf.f_bsize as u64,
        buf.f_blocks as u64,
        buf.f_bfree as u64,
        buf.f_bavail as u64,
        buf.f_files as u64,
        buf.f_ffree as u64,
    ])
}

#[cfg(windows)]
fn statfs(path: &Path) -> io::Result<[u64; 7]> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceW, GetVolumePathNameW};
    let metadata = fs::metadata(path)?;
    let directory = if metadata.is_dir() { path } else { path.parent().unwrap_or(path) };
    let wide: Vec<u16> = directory.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut root = [0u16; 1024];
    let (mut sectors, mut bytes, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    // SAFETY: `wide` is NUL-terminated, `root` has the stated length, and the outputs are writable.
    let ok = unsafe {
        GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) != 0
            && GetDiskFreeSpaceW(root.as_ptr(), &mut sectors, &mut bytes, &mut free, &mut total) != 0
    };
    if !ok {
        return Err(io::Error::last_os_error());
    }
    let (free, total) = (u64::from(free), u64::from(total));
    Ok([0, u64::from(sectors) * u64::from(bytes), total, free, free, 0, 0])
}
