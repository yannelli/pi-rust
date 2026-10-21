//! Natives for `node:fs`: one request type run inline (sync) or on a blocking thread (async).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rquickjs::function::Opt;
use rquickjs::{Ctx, Object, Value};
use serde::Deserialize;

use super::fs_output::{Output, to_js};
use super::set_fn;
use super::{fs_meta, fs_ops};
use crate::error::JsError;
use crate::runtime::convert::{latin1_to_bytes, throw, to_json};
use crate::runtime::scope::spawn_op;
use crate::runtime::state::{Shared, state};

/// Open files by the numbers handed to JavaScript as file descriptors.
pub(crate) struct FileTable {
    next: AtomicU64,
    files: Mutex<HashMap<u64, Arc<Mutex<File>>>>,
}

impl Default for FileTable {
    fn default() -> Self {
        Self { next: AtomicU64::new(3), files: Mutex::default() }
    }
}

impl FileTable {
    fn insert(&self, file: File) -> u64 {
        let fd = self.next.fetch_add(1, Ordering::SeqCst);
        self.files.lock().unwrap_or_else(|poison| poison.into_inner()).insert(fd, Arc::new(Mutex::new(file)));
        fd
    }

    pub(super) fn get(&self, fd: u64, syscall: &str) -> Result<Arc<Mutex<File>>, JsError> {
        let files = self.files.lock().unwrap_or_else(|poison| poison.into_inner());
        files.get(&fd).cloned().ok_or_else(|| {
            JsError::new(format!("EBADF: bad file descriptor, {syscall}"))
                .with_code("EBADF")
                .with_data("syscall", syscall)
        })
    }

    fn remove(&self, fd: u64) -> bool {
        self.files.lock().unwrap_or_else(|poison| poison.into_inner()).remove(&fd).is_some()
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub(crate) enum Request {
    ReadFile {
        path: Option<String>,
        fd: Option<u64>,
    },
    WriteFile {
        path: Option<String>,
        fd: Option<u64>,
        flag: String,
        mode: Option<u32>,
    },
    Open {
        path: String,
        flags: String,
        mode: Option<u32>,
    },
    Close {
        fd: u64,
    },
    Read {
        fd: u64,
        length: usize,
        position: Option<f64>,
    },
    Write {
        fd: u64,
        position: Option<f64>,
    },
    Fsync {
        fd: u64,
    },
    Ftruncate {
        fd: u64,
        len: u64,
    },
    Fstat {
        fd: u64,
    },
    Futimes {
        fd: u64,
        atime: f64,
        mtime: f64,
    },
    Stat {
        path: String,
        follow: bool,
    },
    Exists {
        path: String,
    },
    Access {
        path: String,
        mode: u32,
    },
    Readdir {
        path: String,
        recursive: bool,
    },
    Mkdir {
        path: String,
        recursive: bool,
        mode: Option<u32>,
    },
    Rmdir {
        path: String,
        recursive: bool,
    },
    Rm {
        path: String,
        recursive: bool,
        force: bool,
    },
    Unlink {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
    CopyFile {
        from: String,
        to: String,
        flags: u32,
    },
    Cp {
        from: String,
        to: String,
        recursive: bool,
        force: bool,
        #[serde(rename = "errorOnExist")]
        error_on_exist: bool,
    },
    Realpath {
        path: String,
    },
    Readlink {
        path: String,
    },
    Symlink {
        target: String,
        path: String,
        kind: Option<String>,
    },
    Link {
        from: String,
        to: String,
    },
    Chmod {
        path: String,
        mode: u32,
    },
    Chown {
        path: String,
        uid: u32,
        gid: u32,
        follow: bool,
    },
    Utimes {
        path: String,
        atime: f64,
        mtime: f64,
    },
    Truncate {
        path: String,
        len: u64,
    },
    Mkdtemp {
        prefix: String,
    },
    #[serde(untagged)]
    Meta(fs_meta::MetaRequest),
}

fn display(path: &Path) -> String {
    let text = path.display().to_string();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => rest.to_string(),
        _ => text,
    }
}

fn two_path_error(error: &std::io::Error, syscall: &str, from: &str, to: &str) -> JsError {
    let mut mapped = JsError::from_io(error, syscall, Some(from));
    mapped.message = format!("{} -> '{to}'", mapped.message);
    mapped.with_data("dest", to)
}

fn read_dir(root: &Path, relative: &Path, recursive: bool, out: &mut Vec<(String, u8)>) -> std::io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(root.join(relative))?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let kind = entry.file_type().map(fs_ops::type_code).unwrap_or(0);
        let name = relative.join(entry.file_name());
        out.push((name.display().to_string(), kind));
        if recursive && kind == 2 {
            read_dir(root, &name, true, out)?;
        }
    }
    Ok(())
}

/// Map an I/O error to a Node error naming `syscall` and `path`.
fn io_error<'a>(syscall: &'static str, path: &'a str) -> impl Fn(std::io::Error) -> JsError + 'a {
    move |error| JsError::from_io(&error, syscall, Some(path))
}

fn read_to_end(file: &mut File) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Run one request; `cwd` resolves relative paths, `data` carries bytes for writes.
pub(crate) fn run(request: Request, data: Option<Vec<u8>>, cwd: &Path, shared: &Shared) -> Result<Output, JsError> {
    let files = &shared.files;
    let resolve = |path: &str| -> PathBuf { cwd.join(path) };
    let data = data.unwrap_or_default();
    Ok(match request {
        Request::ReadFile { fd: Some(fd), .. } => {
            let file = files.get(fd, "read")?;
            let mut file = file.lock().unwrap_or_else(|poison| poison.into_inner());
            Output::Bytes(read_to_end(&mut file).map_err(|error| JsError::from_io(&error, "read", None))?)
        }
        Request::ReadFile { path, .. } => {
            let path = path.unwrap_or_default();
            let target = resolve(&path);
            if target.is_dir() {
                return Err(JsError::new("EISDIR: illegal operation on a directory, read")
                    .with_code("EISDIR")
                    .with_data("syscall", "read"));
            }
            Output::Bytes(fs::read(target).map_err(io_error("open", &path))?)
        }
        Request::WriteFile { fd: Some(fd), .. } => {
            let file = files.get(fd, "write")?;
            let mut file = file.lock().unwrap_or_else(|poison| poison.into_inner());
            file.write_all(&data).map_err(|error| JsError::from_io(&error, "write", None))?;
            Output::Undefined
        }
        Request::WriteFile { path, flag, mode, .. } => {
            let path = path.unwrap_or_default();
            let mut file = fs_ops::open_options(&flag, mode)?.open(resolve(&path)).map_err(io_error("open", &path))?;
            file.write_all(&data).map_err(io_error("write", &path))?;
            Output::Undefined
        }
        Request::Open { path, flags, mode } => {
            let file = fs_ops::open_options(&flags, mode)?.open(resolve(&path)).map_err(io_error("open", &path))?;
            Output::Number(files.insert(file) as f64)
        }
        Request::Close { fd } => {
            if !files.remove(fd) {
                files.get(fd, "close")?;
            }
            Output::Undefined
        }
        Request::Read { fd, length, position } => {
            let file = files.get(fd, "read")?;
            let mut file = file.lock().unwrap_or_else(|poison| poison.into_inner());
            let mut buffer = vec![0; length];
            let count = match position.filter(|position| *position >= 0.0) {
                Some(position) => read_at(&mut file, &mut buffer, position as u64),
                None => file.read(&mut buffer),
            };
            let count = count.map_err(|error| JsError::from_io(&error, "read", None))?;
            buffer.truncate(count);
            Output::Bytes(buffer)
        }
        Request::Write { fd, position } => {
            let file = files.get(fd, "write")?;
            let mut file = file.lock().unwrap_or_else(|poison| poison.into_inner());
            if let Some(position) = position.filter(|position| *position >= 0.0) {
                file.seek(SeekFrom::Start(position as u64)).map_err(|error| JsError::from_io(&error, "write", None))?;
            }
            file.write_all(&data).map_err(|error| JsError::from_io(&error, "write", None))?;
            Output::Number(data.len() as f64)
        }
        Request::Fsync { fd } => {
            let file = files.get(fd, "fsync")?;
            let file = file.lock().unwrap_or_else(|poison| poison.into_inner());
            file.sync_all().map_err(|error| JsError::from_io(&error, "fsync", None))?;
            Output::Undefined
        }
        Request::Ftruncate { fd, len } => {
            let file = files.get(fd, "ftruncate")?;
            let file = file.lock().unwrap_or_else(|poison| poison.into_inner());
            file.set_len(len).map_err(|error| JsError::from_io(&error, "ftruncate", None))?;
            Output::Undefined
        }
        Request::Fstat { fd } => {
            let file = files.get(fd, "fstat")?;
            let file = file.lock().unwrap_or_else(|poison| poison.into_inner());
            Output::Stat(Box::new(fs_ops::stat_info(
                &file.metadata().map_err(|error| JsError::from_io(&error, "fstat", None))?,
            )))
        }
        Request::Futimes { fd, atime, mtime } => {
            let file = files.get(fd, "futime")?;
            let file = file.lock().unwrap_or_else(|poison| poison.into_inner());
            fs_ops::futimes(&file, atime, mtime).map_err(|error| JsError::from_io(&error, "futime", None))?;
            Output::Undefined
        }
        Request::Stat { path, follow } => {
            let target = resolve(&path);
            let metadata = if follow { fs::metadata(target) } else { fs::symlink_metadata(target) };
            Output::Stat(Box::new(fs_ops::stat_info(
                &metadata.map_err(io_error(if follow { "stat" } else { "lstat" }, &path))?,
            )))
        }
        Request::Exists { path } => Output::Bool(resolve(&path).exists()),
        Request::Access { path, mode } => {
            fs_ops::access(&resolve(&path), mode).map_err(io_error("access", &path))?;
            Output::Undefined
        }
        Request::Readdir { path, recursive } => {
            let mut entries = Vec::new();
            read_dir(&resolve(&path), Path::new(""), recursive, &mut entries).map_err(io_error("scandir", &path))?;
            Output::Entries(entries)
        }
        Request::Mkdir { path, recursive: true, mode } => {
            let target = resolve(&path);
            if target.exists() && !target.is_dir() {
                return Err(JsError::from_io(
                    &std::io::Error::from(std::io::ErrorKind::AlreadyExists),
                    "mkdir",
                    Some(&path),
                ));
            }
            Output::MaybeText(
                fs_ops::mkdir_recursive(&target, mode).map_err(io_error("mkdir", &path))?.map(|dir| display(&dir)),
            )
        }
        Request::Mkdir { path, mode, .. } => {
            fs_ops::dir_builder(mode, false).create(resolve(&path)).map_err(io_error("mkdir", &path))?;
            Output::Undefined
        }
        Request::Rmdir { path, recursive } => {
            let target = resolve(&path);
            let result = if recursive { fs::remove_dir_all(target) } else { fs::remove_dir(target) };
            result.map_err(io_error("rmdir", &path))?;
            Output::Undefined
        }
        Request::Rm { path, recursive, force } => {
            fs_ops::remove(&resolve(&path), &path, recursive, force)?;
            Output::Undefined
        }
        Request::Unlink { path } => {
            fs::remove_file(resolve(&path)).map_err(io_error("unlink", &path))?;
            Output::Undefined
        }
        Request::Rename { from, to } => {
            fs::rename(resolve(&from), resolve(&to)).map_err(|error| two_path_error(&error, "rename", &from, &to))?;
            Output::Undefined
        }
        Request::CopyFile { from, to, flags } => {
            let target = resolve(&to);
            if flags & 1 != 0 && target.exists() {
                let error = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
                return Err(two_path_error(&error, "copyfile", &from, &to));
            }
            fs::copy(resolve(&from), target).map_err(|error| two_path_error(&error, "copyfile", &from, &to))?;
            Output::Undefined
        }
        Request::Cp { from, to, recursive, force, error_on_exist } => {
            fs_ops::copy_tree(&resolve(&from), &resolve(&to), recursive, force, error_on_exist)
                .map_err(|error| two_path_error(&error, "cp", &from, &to))?;
            Output::Undefined
        }
        Request::Realpath { path } => {
            Output::Text(display(&fs::canonicalize(resolve(&path)).map_err(io_error("realpath", &path))?))
        }
        Request::Readlink { path } => {
            Output::Text(display(&fs::read_link(resolve(&path)).map_err(io_error("readlink", &path))?))
        }
        Request::Symlink { target, path, kind } => {
            fs_ops::symlink(Path::new(&target), &resolve(&path), kind.as_deref())
                .map_err(|error| two_path_error(&error, "symlink", &target, &path))?;
            Output::Undefined
        }
        Request::Link { from, to } => {
            fs::hard_link(resolve(&from), resolve(&to)).map_err(|error| two_path_error(&error, "link", &from, &to))?;
            Output::Undefined
        }
        Request::Chmod { path, mode } => {
            fs_ops::chmod(&resolve(&path), mode).map_err(io_error("chmod", &path))?;
            Output::Undefined
        }
        Request::Chown { path, uid, gid, follow } => {
            fs_ops::chown(&resolve(&path), uid, gid, follow).map_err(io_error("chown", &path))?;
            Output::Undefined
        }
        Request::Utimes { path, atime, mtime } => {
            fs_ops::utimes(&resolve(&path), atime, mtime).map_err(io_error("utime", &path))?;
            Output::Undefined
        }
        Request::Truncate { path, len } => {
            let file = fs::OpenOptions::new().write(true).open(resolve(&path)).map_err(io_error("open", &path))?;
            file.set_len(len).map_err(io_error("ftruncate", &path))?;
            Output::Undefined
        }
        Request::Mkdtemp { prefix } => {
            let path = fs_ops::mkdtemp(&display(&resolve(&prefix)))
                .map_err(io_error("mkdtemp", &format!("{prefix}XXXXXX")))?;
            Output::Text(display(&path))
        }
        Request::Meta(request) => fs_meta::run(request, cwd, files)?,
    })
}

#[cfg(unix)]
fn read_at(file: &mut File, buffer: &mut [u8], position: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, position)
}

#[cfg(windows)]
fn read_at(file: &mut File, buffer: &mut [u8], position: u64) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, position)
}

fn parse<'js>(ctx: &Ctx<'js>, request: Value<'js>) -> rquickjs::Result<Request> {
    let json = to_json(ctx, request).map_err(|error| throw(ctx, &error))?;
    serde_json::from_value(json)
        .map_err(|error| throw(ctx, &JsError::type_error(format!("Invalid fs request: {error}"))))
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "sync", |ctx: Ctx<'js>, request: Value<'js>, data: Opt<Option<String>>| {
        let request = parse(&ctx, request)?;
        let data = data.0.flatten().map(|text| latin1_to_bytes(&text));
        let state = state();
        let cwd = state.cwd.borrow().clone();
        let output = run(request, data, &cwd, &state.shared).map_err(|error| throw(&ctx, &error))?;
        to_js(&ctx, output).map_err(|error| throw(&ctx, &error))
    })?;
    set_fn(&object, "async", |ctx: Ctx<'js>, request: Value<'js>, data: Opt<Option<String>>| {
        let request = parse(&ctx, request)?;
        let data = data.0.flatten().map(|text| latin1_to_bytes(&text));
        let state = state();
        let cwd = state.cwd.borrow().clone();
        let shared = state.shared.clone();
        let work = async move {
            tokio::task::spawn_blocking(move || run(request, data, &cwd, &shared))
                .await
                .map_err(|error| JsError::new(format!("fs task failed: {error}")))?
        };
        spawn_op(&ctx, work, to_js)
    })?;
    Ok(object)
}
