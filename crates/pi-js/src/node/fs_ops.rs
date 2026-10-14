//! File system operations behind `node:fs`, independent of the JavaScript engine.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::error::JsError;

/// `fs.Stats` fields, all as numbers the way Node exposes them.
#[derive(Debug, Default)]
pub(crate) struct StatInfo {
    pub dev: f64,
    pub ino: f64,
    pub mode: f64,
    pub nlink: f64,
    pub uid: f64,
    pub gid: f64,
    pub rdev: f64,
    pub size: f64,
    pub blksize: f64,
    pub blocks: f64,
    pub atime_ms: f64,
    pub mtime_ms: f64,
    pub ctime_ms: f64,
    pub birthtime_ms: f64,
    /// Exact `dev`, `ino`, `rdev`, `size`, `blocks`, and the four times in nanoseconds, for
    /// `bigint: true` stats.
    pub exact: [i128; 9],
}

#[cfg(not(unix))]
const S_IFREG: u32 = 0o100_000;
#[cfg(not(unix))]
const S_IFDIR: u32 = 0o040_000;
#[cfg(not(unix))]
const S_IFLNK: u32 = 0o120_000;

fn nanos(time: io::Result<std::time::SystemTime>) -> i128 {
    match time {
        Ok(time) => match time.duration_since(std::time::UNIX_EPOCH) {
            Ok(duration) => duration.as_nanos() as i128,
            Err(before) => -(before.duration().as_nanos() as i128),
        },
        Err(_) => 0,
    }
}

pub(crate) fn stat_info(metadata: &Metadata) -> StatInfo {
    let (atime, mtime, birthtime) = (nanos(metadata.accessed()), nanos(metadata.modified()), nanos(metadata.created()));
    let mut info = StatInfo {
        size: metadata.len() as f64,
        atime_ms: atime as f64 / 1e6,
        mtime_ms: mtime as f64 / 1e6,
        birthtime_ms: birthtime as f64 / 1e6,
        exact: [0, 0, 0, i128::from(metadata.len()), 0, atime, mtime, 0, birthtime],
        ..StatInfo::default()
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        info.dev = metadata.dev() as f64;
        info.ino = metadata.ino() as f64;
        info.mode = f64::from(metadata.mode());
        info.nlink = metadata.nlink() as f64;
        info.uid = f64::from(metadata.uid());
        info.gid = f64::from(metadata.gid());
        info.rdev = metadata.rdev() as f64;
        info.blksize = metadata.blksize() as f64;
        info.blocks = metadata.blocks() as f64;
        info.ctime_ms = metadata.ctime() as f64 * 1000.0 + metadata.ctime_nsec() as f64 / 1e6;
        info.exact[0] = i128::from(metadata.dev());
        info.exact[1] = i128::from(metadata.ino());
        info.exact[2] = i128::from(metadata.rdev());
        info.exact[4] = i128::from(metadata.blocks());
        info.exact[7] = i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec());
    }
    #[cfg(not(unix))]
    {
        let kind = if metadata.is_symlink() {
            S_IFLNK
        } else if metadata.is_dir() {
            S_IFDIR | 0o111
        } else {
            S_IFREG
        };
        let permissions = if metadata.permissions().readonly() { 0o444 } else { 0o666 };
        info.mode = f64::from(kind | permissions);
        info.nlink = 1.0;
        info.ctime_ms = info.mtime_ms;
        info.exact[7] = info.exact[6];
    }
    if info.birthtime_ms == 0.0 {
        info.birthtime_ms = info.ctime_ms;
        info.exact[8] = info.exact[7];
    }
    info
}

/// The `S_IF*` bits for a file type, used for `Dirent`.
pub(crate) fn type_code(file_type: fs::FileType) -> u8 {
    if file_type.is_symlink() {
        3
    } else if file_type.is_dir() {
        2
    } else if file_type.is_file() {
        1
    } else {
        other_type_code(file_type)
    }
}

#[cfg(unix)]
fn other_type_code(file_type: fs::FileType) -> u8 {
    use std::os::unix::fs::FileTypeExt;
    if file_type.is_fifo() {
        4
    } else if file_type.is_socket() {
        5
    } else if file_type.is_char_device() {
        6
    } else if file_type.is_block_device() {
        7
    } else {
        0
    }
}

#[cfg(not(unix))]
fn other_type_code(_file_type: fs::FileType) -> u8 {
    0
}

/// Open options for a Node flag string such as `r`, `w+`, `ax`, or `as+`.
pub(crate) fn open_options(flags: &str, mode: Option<u32>) -> Result<OpenOptions, JsError> {
    let mut options = OpenOptions::new();
    let flags = flags.replace('s', "");
    match flags.as_str() {
        "r" => options.read(true),
        "r+" => options.read(true).write(true),
        "w" => options.write(true).create(true).truncate(true),
        "wx" | "xw" => options.write(true).create_new(true),
        "w+" => options.read(true).write(true).create(true).truncate(true),
        "wx+" | "xw+" => options.read(true).write(true).create_new(true),
        "a" => options.append(true).create(true),
        "ax" | "xa" => options.append(true).create_new(true),
        "a+" => options.read(true).append(true).create(true),
        "ax+" | "xa+" => options.read(true).append(true).create_new(true),
        _ => {
            return Err(JsError::type_error(format!("The value \"{flags}\" is invalid for option \"flags\""))
                .with_code("ERR_INVALID_ARG_VALUE"));
        }
    };
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(options)
}

/// `mkdir -p`; returns the first directory created, as Node's `recursive` mkdir does.
pub(crate) fn mkdir_recursive(path: &Path, mode: Option<u32>) -> io::Result<Option<PathBuf>> {
    if path.is_dir() {
        return Ok(None);
    }
    let mut first = path.to_path_buf();
    while let Some(parent) = first.parent() {
        if parent.as_os_str().is_empty() || parent.exists() {
            break;
        }
        first = parent.to_path_buf();
    }
    dir_builder(mode, true).create(path)?;
    Ok(Some(first))
}

pub(crate) fn dir_builder(mode: Option<u32>, recursive: bool) -> fs::DirBuilder {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    builder
}

/// `fs.rm`: files, links, and (with `recursive`) directories; `force` ignores missing paths.
pub(crate) fn remove(path: &Path, display: &str, recursive: bool, force: bool) -> Result<(), JsError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if force && error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(JsError::from_io(&error, "rm", Some(display))),
    };
    let result = if metadata.is_dir() {
        if !recursive {
            return Err(JsError::new(format!("Path is a directory: rm returned EISDIR (is a directory) {display}"))
                .with_code("ERR_FS_EISDIR")
                .with_data("path", display));
        }
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|error| JsError::from_io(&error, "rm", Some(display)))
}

/// `fs.cp`: copy a file, or a directory tree when `recursive`.
pub(crate) fn copy_tree(from: &Path, to: &Path, recursive: bool, force: bool, error_on_exist: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    if metadata.is_dir() {
        if !recursive {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                "Recursive option is required to copy a directory",
            ));
        }
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()), recursive, force, error_on_exist)?;
        }
        return Ok(());
    }
    if to.exists() {
        if !force {
            if error_on_exist {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            return Ok(());
        }
        fs::remove_file(to)?;
    }
    if metadata.is_symlink() {
        return symlink(&fs::read_link(from)?, to, None);
    }
    fs::copy(from, to).map(|_| ())
}

/// Create a symbolic link. On Windows `kind` (`dir`, `junction`, `file`) or the target picks the kind.
pub(crate) fn symlink(target: &Path, path: &Path, kind: Option<&str>) -> io::Result<()> {
    #[cfg(unix)]
    {
        let _ = kind;
        std::os::unix::fs::symlink(target, path)
    }
    #[cfg(windows)]
    {
        let resolved = path.parent().map_or_else(|| target.to_path_buf(), |parent| parent.join(target));
        let is_dir = matches!(kind, Some("dir") | Some("junction")) || (kind.is_none() && resolved.is_dir());
        if is_dir {
            std::os::windows::fs::symlink_dir(target, path)
        } else {
            std::os::windows::fs::symlink_file(target, path)
        }
    }
}

/// `fs.mkdtemp`: the prefix plus six random characters.
pub(crate) fn mkdtemp(prefix: &str) -> io::Result<PathBuf> {
    use ring::rand::SecureRandom;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let random = ring::rand::SystemRandom::new();
    for _ in 0..100 {
        let mut bytes = [0u8; 6];
        random.fill(&mut bytes).map_err(|_| io::Error::other("random generator failed"))?;
        let suffix: String = bytes.iter().map(|byte| CHARS[usize::from(*byte) % CHARS.len()] as char).collect();
        let path = PathBuf::from(format!("{prefix}{suffix}"));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::from(io::ErrorKind::AlreadyExists))
}

pub(crate) fn chmod(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(mode & 0o200 == 0);
        fs::set_permissions(path, permissions)
    }
}

pub(crate) fn chown(path: &Path, uid: u32, gid: u32, follow: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        if follow {
            std::os::unix::fs::chown(path, Some(uid), Some(gid))
        } else {
            std::os::unix::fs::lchown(path, Some(uid), Some(gid))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, uid, gid, follow);
        Ok(())
    }
}

fn system_time(seconds: f64) -> std::time::SystemTime {
    let duration = std::time::Duration::from_secs_f64(seconds.abs());
    if seconds >= 0.0 { std::time::UNIX_EPOCH + duration } else { std::time::UNIX_EPOCH - duration }
}

/// `fs.utimes` with times in seconds.
pub(crate) fn utimes(path: &Path, atime: f64, mtime: f64) -> io::Result<()> {
    let file = if path.is_dir() { File::open(path)? } else { OpenOptions::new().write(true).open(path)? };
    let times = fs::FileTimes::new().set_accessed(system_time(atime)).set_modified(system_time(mtime));
    file.set_times(times)
}

pub(crate) fn futimes(file: &File, atime: f64, mtime: f64) -> io::Result<()> {
    file.set_times(fs::FileTimes::new().set_accessed(system_time(atime)).set_modified(system_time(mtime)))
}

/// `fs.access` for `F_OK` (0), `R_OK` (4), `W_OK` (2), and `X_OK` (1).
pub(crate) fn access(path: &Path, mode: u32) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    let denied = || io::Error::from(io::ErrorKind::PermissionDenied);
    if mode & 2 != 0 && metadata.permissions().readonly() {
        return Err(denied());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bits = metadata.permissions().mode();
        if mode & 1 != 0 && bits & 0o111 == 0 {
            return Err(denied());
        }
        if mode & 4 != 0 && bits & 0o444 == 0 {
            return Err(denied());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_flags_and_creates_directories() {
        assert!(open_options("r", None).is_ok());
        assert!(open_options("as+", None).is_ok());
        assert!(open_options("q", None).is_err());
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        let first = mkdir_recursive(&nested, None).unwrap().unwrap();
        assert_eq!(first, dir.path().join("a"));
        assert_eq!(mkdir_recursive(&nested, None).unwrap(), None);
        let temp = mkdtemp(&format!("{}/tmp-", dir.path().display())).unwrap();
        assert!(temp.is_dir() && temp.file_name().unwrap().len() == 10);
        fs::write(nested.join("f.txt"), "x").unwrap();
        copy_tree(&dir.path().join("a"), &dir.path().join("copy"), true, false, false).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("copy/b/c/f.txt")).unwrap(), "x");
        assert!(remove(&dir.path().join("copy"), "copy", false, false).is_err());
        remove(&dir.path().join("copy"), "copy", true, false).unwrap();
        remove(&dir.path().join("missing"), "missing", false, true).unwrap();
    }
}
