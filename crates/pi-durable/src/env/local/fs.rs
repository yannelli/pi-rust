//! The `FileSystem` implementation of `LocalExecutionEnv`.

use std::fs::{File, OpenOptions};
use std::hash::BuildHasher;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use pi_chord::Context;

use super::paths::{aborted, blocking, file_info, io_error, join_path};
use super::readers::{LocalBinaryReader, LocalDirReader, LocalTextLineReader, MAX_SAFE_INTEGER};
use super::{LOCAL_ENV_ID, LocalExecutionEnv, process, watch};
use crate::env::{
    BinaryReader, DirReader, FileError, FileErrorCode, FileInfo, FileResult, FileSystem, FileWatcher, RemoveOptions,
    TempFileOptions, TextLineReader, WatchListener, WatchTarget,
};

fn check(context: &Context, path: &str) -> FileResult<()> {
    if context.is_cancelled() { Err(aborted(path)) } else { Ok(()) }
}

fn random_u64() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::collections::hash_map::RandomState::new().hash_one(COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Six random letters and digits, like `mkdtemp`.
fn random_suffix() -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut value = random_u64();
    (0..6)
        .map(|_| {
            let char = CHARS[(value % CHARS.len() as u64) as usize] as char;
            value /= CHARS.len() as u64;
            char
        })
        .collect()
}

/// A random version 4 UUID.
fn random_uuid() -> String {
    let (high, low) = (random_u64(), random_u64());
    let high = (high & 0xffff_ffff_ffff_0fff) | 0x4000;
    let low = (low & 0x3fff_ffff_ffff_ffff) | 0x8000_0000_0000_0000;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        high >> 32,
        (high >> 16) & 0xffff,
        high & 0xffff,
        low >> 48,
        low & 0xffff_ffff_ffff
    )
}

fn symlink_refused(path: &str, cause: Option<io::Error>) -> FileError {
    let error = FileError::new(FileErrorCode::Invalid, "Refusing to follow a symbolic link", Some(path.to_string()));
    match cause {
        Some(cause) => error.with_cause(std::sync::Arc::new(cause)),
        None => error,
    }
}

#[cfg(unix)]
fn open_for_binary_reads(path: &str, no_follow: bool) -> FileResult<File> {
    use rustix::fs::OFlags;
    use std::os::unix::fs::OpenOptionsExt;
    // Nonblocking, so opening a FIFO does not wait for a writer.
    let mut flags = OFlags::NONBLOCK;
    if no_follow {
        flags |= OFlags::NOFOLLOW;
    }
    match OpenOptions::new().read(true).custom_flags(flags.bits() as i32).open(path) {
        Ok(file) => Ok(file),
        Err(error) => {
            // O_NOFOLLOW reports a final-component symlink as ELOOP (EMLINK on some BSDs).
            let raw = error.raw_os_error();
            let loops = [rustix::io::Errno::LOOP, rustix::io::Errno::MLINK].map(|errno| Some(errno.raw_os_error()));
            if no_follow && loops.contains(&raw) {
                return Err(symlink_refused(path, Some(error)));
            }
            Err(io_error(error, path))
        }
    }
}

#[cfg(not(unix))]
fn open_for_binary_reads(path: &str, no_follow: bool) -> FileResult<File> {
    // Windows has no O_NOFOLLOW; check the final component before opening (not race-free).
    let metadata = std::fs::symlink_metadata(path).map_err(|error| io_error(error, path))?;
    if no_follow && metadata.file_type().is_symlink() {
        return Err(symlink_refused(path, None));
    }
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
        let message = "EISDIR: illegal operation on a directory, read";
        return Err(FileError::new(FileErrorCode::IsDirectory, message, Some(path.to_string())));
    }
    File::open(path).map_err(|error| io_error(error, path))
}

fn create_parent(path: &str) -> io::Result<()> {
    match Path::new(path).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => std::fs::create_dir_all(parent),
        _ => Ok(()),
    }
}

fn remove_path(path: &str, options: RemoveOptions) -> io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound && options.force => return Ok(()),
        other => other?,
    };
    if metadata.is_dir() {
        if !options.recursive {
            // Node fails with ERR_FS_EISDIR, which its error mapping reports as `unknown`.
            return Err(io::Error::other(format!("Path is a directory: rm returned EISDIR (is a directory) {path}")));
        }
        return std::fs::remove_dir_all(path);
    }
    match std::fs::remove_file(path) {
        // A Windows symbolic link to a directory is removed as a directory.
        Err(error) if cfg!(windows) && metadata.file_type().is_symlink() => std::fs::remove_dir(path).or(Err(error)),
        other => other,
    }
}

/// `realpath`, without the `\\?\` prefix Windows adds to canonical paths.
fn canonicalize(path: &str) -> io::Result<String> {
    let canonical = std::fs::canonicalize(path)?.to_string_lossy().into_owned();
    if cfg!(windows) {
        if let Some(rest) = canonical.strip_prefix(r"\\?\") {
            if !rest.starts_with("UNC\\") {
                return Ok(rest.to_string());
            }
        }
    }
    Ok(canonical)
}

impl LocalExecutionEnv {
    fn temp_root(&self) -> PathBuf {
        self.temp_dir.as_ref().map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
    }

    /// Run blocking work on `path` once the context allows it, mapping I/O errors to `path`.
    async fn run<T: Send + 'static>(
        &self,
        path: String,
        context: &Context,
        work: impl FnOnce(&str) -> io::Result<T> + Send + 'static,
    ) -> FileResult<T> {
        check(context, &path)?;
        blocking(move || work(&path).map_err(|error| io_error(error, &path))).await
    }
}

#[async_trait]
impl FileSystem for LocalExecutionEnv {
    fn id(&self) -> &str {
        LOCAL_ENV_ID
    }

    fn cwd(&self) -> String {
        self.current_cwd()
    }

    async fn absolute_path(&self, path: &str, _context: &Context) -> FileResult<String> {
        Ok(self.resolve(path))
    }

    async fn join_path(&self, parts: &[String], _context: &Context) -> FileResult<String> {
        Ok(join_path(parts))
    }

    async fn read_text_file(&self, path: &str, context: &Context) -> FileResult<String> {
        let bytes = self.read_binary_file(path, context).await?;
        Ok(String::from_utf8(bytes).unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()))
    }

    async fn open_text_line_reader(&self, path: &str, context: &Context) -> FileResult<Box<dyn TextLineReader>> {
        let resolved = self.resolve(path);
        let file = self.run(resolved.clone(), context, |path| File::open(path)).await?;
        check(context, &resolved)?;
        Ok(Box::new(LocalTextLineReader::new(file, resolved)))
    }

    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        context: &Context,
    ) -> FileResult<Vec<String>> {
        if max_lines == Some(0) {
            return Ok(Vec::new());
        }
        let reader = self.open_text_line_reader(path, context).await?;
        let mut lines = Vec::new();
        let result = loop {
            if max_lines.is_some_and(|max| lines.len() >= max) {
                break Ok(lines);
            }
            match reader.read_line(context).await {
                Ok(Some(line)) => lines.push(line.text),
                Ok(None) => break Ok(lines),
                Err(error) => break Err(error),
            }
        };
        reader.close(context).await;
        result
    }

    async fn read_binary_file(&self, path: &str, context: &Context) -> FileResult<Vec<u8>> {
        let resolved = self.resolve(path);
        let bytes = self.run(resolved.clone(), context, |path| std::fs::read(path)).await?;
        check(context, &resolved)?;
        Ok(bytes)
    }

    async fn open_binary_reader(
        &self,
        path: &str,
        no_follow: bool,
        context: &Context,
    ) -> FileResult<Box<dyn BinaryReader>> {
        let resolved = self.resolve(path);
        check(context, &resolved)?;
        let opening = resolved.clone();
        let file = blocking(move || {
            let file = open_for_binary_reads(&opening, no_follow)?;
            let metadata = file.metadata().map_err(|error| io_error(error, &opening))?;
            if metadata.is_file() {
                return Ok(file);
            }
            Err(if metadata.is_dir() {
                let message = "EISDIR: illegal operation on a directory, read";
                FileError::new(FileErrorCode::IsDirectory, message, Some(opening))
            } else {
                FileError::new(FileErrorCode::Invalid, "Not a regular file", Some(opening))
            })
        })
        .await?;
        check(context, &resolved)?;
        Ok(Box::new(LocalBinaryReader::new(file, resolved)))
    }

    async fn write_file(&self, path: &str, content: &[u8], context: &Context) -> FileResult<()> {
        let resolved = self.resolve(path);
        self.run(resolved.clone(), context, create_parent).await?;
        let content = content.to_vec();
        self.run(resolved, context, move |path| std::fs::write(path, content)).await
    }

    async fn append_file(&self, path: &str, content: &[u8], context: &Context) -> FileResult<()> {
        let resolved = self.resolve(path);
        self.run(resolved.clone(), context, create_parent).await?;
        let content = content.to_vec();
        self.run(resolved.clone(), context, move |path| {
            OpenOptions::new().append(true).create(true).open(path)?.write_all(&content)
        })
        .await?;
        check(context, &resolved)
    }

    async fn truncate_file(&self, path: &str, size: u64, context: &Context) -> FileResult<()> {
        let resolved = self.resolve(path);
        check(context, &resolved)?;
        if size > MAX_SAFE_INTEGER {
            let message = "File size must be a non-negative safe integer";
            return Err(FileError::new(FileErrorCode::Invalid, message, Some(resolved)));
        }
        self.run(resolved.clone(), context, move |path| OpenOptions::new().write(true).open(path)?.set_len(size))
            .await?;
        check(context, &resolved)
    }

    async fn flush_file(&self, path: &str, context: &Context) -> FileResult<()> {
        let resolved = self.resolve(path);
        check(context, &resolved)?;
        let flushing = resolved.clone();
        blocking(move || {
            // POSIX refuses to open a directory for writing; Windows does not, so check explicitly.
            let metadata = std::fs::metadata(&flushing).map_err(|error| io_error(error, &flushing))?;
            if metadata.is_dir() {
                return Err(FileError::new(FileErrorCode::IsDirectory, "Is a directory", Some(flushing)));
            }
            let file = OpenOptions::new().read(true).write(true).open(&flushing);
            file.and_then(|file| file.sync_all()).map_err(|error| io_error(error, &flushing))
        })
        .await?;
        check(context, &resolved)
    }

    async fn rename_file(&self, source: &str, destination: &str, context: &Context) -> FileResult<()> {
        let (source, destination) = (self.resolve(source), self.resolve(destination));
        check(context, &destination)?;
        self.run(source, context, move |source| std::fs::rename(source, destination)).await
    }

    async fn file_info(&self, path: &str, context: &Context) -> FileResult<FileInfo> {
        let resolved = self.resolve(path);
        let metadata = self.run(resolved.clone(), context, |path| std::fs::symlink_metadata(path)).await?;
        file_info(&resolved, &metadata)
    }

    async fn list_dir(&self, path: &str, context: &Context) -> FileResult<Vec<FileInfo>> {
        let resolved = self.resolve(path);
        check(context, &resolved)?;
        let context = context.clone();
        blocking(move || {
            let entries = std::fs::read_dir(&resolved).map_err(|error| io_error(error, &resolved))?;
            let mut infos = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|error| io_error(error, &resolved))?;
                check(&context, &resolved)?;
                let entry_path = Path::new(&resolved).join(entry.file_name()).to_string_lossy().into_owned();
                let metadata = std::fs::symlink_metadata(&entry_path).map_err(|error| io_error(error, &entry_path))?;
                if let Ok(info) = file_info(&entry_path, &metadata) {
                    infos.push(info);
                }
            }
            // libuv sorts directory listings by name bytes on Unix.
            if cfg!(unix) {
                infos.sort_by(|a, b| a.name.cmp(&b.name));
            }
            Ok(infos)
        })
        .await
    }

    async fn open_dir_reader(&self, path: &str, context: &Context) -> FileResult<Box<dyn DirReader>> {
        let resolved = self.resolve(path);
        let entries = self.run(resolved.clone(), context, |path| std::fs::read_dir(path)).await?;
        check(context, &resolved)?;
        Ok(Box::new(LocalDirReader::new(entries, resolved)))
    }

    async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: WatchListener,
        context: &Context,
    ) -> FileResult<Box<dyn FileWatcher>> {
        check(context, &self.current_cwd())?;
        let cwd = self.current_cwd();
        let resolved: Vec<WatchTarget> = targets
            .iter()
            .map(|target| WatchTarget { path: super::paths::resolve_path(&cwd, &target.path), ..target.clone() })
            .collect();
        let watcher = watch::LocalFileWatcher::open(resolved, on_change, self.watch_options).await?;
        if context.is_cancelled() {
            watcher.close(context).await;
            return Err(FileError::aborted(None));
        }
        Ok(Box::new(watcher))
    }

    async fn canonical_path(&self, path: &str, context: &Context) -> FileResult<String> {
        self.run(self.resolve(path), context, canonicalize).await
    }

    async fn exists(&self, path: &str, context: &Context) -> FileResult<bool> {
        match self.file_info(path, context).await {
            Ok(_) => Ok(true),
            Err(error) if error.code == FileErrorCode::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn create_dir(&self, path: &str, recursive: bool, context: &Context) -> FileResult<()> {
        self.run(self.resolve(path), context, move |path| {
            if recursive { std::fs::create_dir_all(path) } else { std::fs::create_dir(path) }
        })
        .await
    }

    async fn remove(&self, path: &str, options: RemoveOptions, context: &Context) -> FileResult<()> {
        self.run(self.resolve(path), context, move |path| remove_path(path, options)).await
    }

    async fn create_temp_dir(&self, prefix: Option<&str>, context: &Context) -> FileResult<String> {
        let root = self.temp_root();
        let prefix = prefix.unwrap_or("tmp-").to_string();
        let base = root.join(&prefix).to_string_lossy().into_owned();
        self.run(base, context, move |base| {
            loop {
                let candidate = format!("{base}{}", random_suffix());
                match std::fs::create_dir(&candidate) {
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    other => return other.map(|()| candidate),
                }
            }
        })
        .await
    }

    async fn create_temp_file(&self, options: TempFileOptions, context: &Context) -> FileResult<String> {
        let dir = self.create_temp_dir(Some("tmp-"), context).await?;
        let name = format!(
            "{}{}{}",
            options.prefix.as_deref().unwrap_or(""),
            random_uuid(),
            options.suffix.as_deref().unwrap_or("")
        );
        let path = Path::new(&dir).join(name).to_string_lossy().into_owned();
        let creating = path.clone();
        blocking(move || std::fs::write(&creating, b"").map_err(|error| io_error(error, &creating))).await?;
        Ok(path)
    }

    async fn cleanup(&self, _context: &Context) {
        let pids: Vec<u32> = self.active_pids.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).drain().collect();
        for pid in pids {
            process::kill_process_tree(pid);
        }
    }
}
