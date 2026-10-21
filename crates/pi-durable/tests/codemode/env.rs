//! A local environment whose file writes and temp directories a test can intercept (upstream subclasses of
//! `NodeExecutionEnv`): `after_write` runs after a write with its path and the call's context, `before_temp_dir`
//! before a temp directory is made.

use async_trait::async_trait;
use futures::future::BoxFuture;
use pi_durable::env::*;

use super::*;

pub type AfterWrite = Arc<dyn Fn(String, Context) -> BoxFuture<'static, ()> + Send + Sync>;
pub type BeforeTempDir = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

pub struct HookedEnv {
    pub inner: LocalExecutionEnv,
    pub after_write: Option<AfterWrite>,
    pub before_temp_dir: Option<BeforeTempDir>,
}

impl HookedEnv {
    pub fn new(cwd: &std::path::Path) -> Self {
        let inner = LocalExecutionEnv::new(LocalEnvOptions::new(cwd.to_string_lossy().into_owned()));
        Self { inner, after_write: None, before_temp_dir: None }
    }
}

/// A plain local environment in `cwd`.
pub fn local_env(cwd: &std::path::Path) -> SharedEnv {
    Arc::new(HookedEnv::new(cwd))
}

#[async_trait]
impl FileSystem for HookedEnv {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn cwd(&self) -> String {
        self.inner.cwd()
    }
    async fn absolute_path(&self, path: &str, c: &Context) -> FileResult<String> {
        self.inner.absolute_path(path, c).await
    }
    async fn join_path(&self, parts: &[String], c: &Context) -> FileResult<String> {
        self.inner.join_path(parts, c).await
    }
    async fn read_text_file(&self, path: &str, c: &Context) -> FileResult<String> {
        self.inner.read_text_file(path, c).await
    }
    async fn open_text_line_reader(&self, path: &str, c: &Context) -> FileResult<Box<dyn TextLineReader>> {
        self.inner.open_text_line_reader(path, c).await
    }
    async fn read_text_lines(&self, path: &str, max: Option<usize>, c: &Context) -> FileResult<Vec<String>> {
        self.inner.read_text_lines(path, max, c).await
    }
    async fn read_binary_file(&self, path: &str, c: &Context) -> FileResult<Vec<u8>> {
        self.inner.read_binary_file(path, c).await
    }
    async fn open_binary_reader(&self, path: &str, no_follow: bool, c: &Context) -> FileResult<Box<dyn BinaryReader>> {
        self.inner.open_binary_reader(path, no_follow, c).await
    }
    async fn write_file(&self, path: &str, content: &[u8], c: &Context) -> FileResult<()> {
        let result = self.inner.write_file(path, content, c).await;
        if let Some(hook) = &self.after_write {
            hook(path.to_owned(), c.clone()).await;
        }
        result
    }
    async fn append_file(&self, path: &str, content: &[u8], c: &Context) -> FileResult<()> {
        self.inner.append_file(path, content, c).await
    }
    async fn truncate_file(&self, path: &str, size: u64, c: &Context) -> FileResult<()> {
        self.inner.truncate_file(path, size, c).await
    }
    async fn flush_file(&self, path: &str, c: &Context) -> FileResult<()> {
        self.inner.flush_file(path, c).await
    }
    async fn rename_file(&self, source: &str, destination: &str, c: &Context) -> FileResult<()> {
        self.inner.rename_file(source, destination, c).await
    }
    async fn file_info(&self, path: &str, c: &Context) -> FileResult<FileInfo> {
        self.inner.file_info(path, c).await
    }
    async fn list_dir(&self, path: &str, c: &Context) -> FileResult<Vec<FileInfo>> {
        self.inner.list_dir(path, c).await
    }
    async fn open_dir_reader(&self, path: &str, c: &Context) -> FileResult<Box<dyn DirReader>> {
        self.inner.open_dir_reader(path, c).await
    }
    async fn watch(&self, t: &[WatchTarget], l: WatchListener, c: &Context) -> FileResult<Box<dyn FileWatcher>> {
        self.inner.watch(t, l, c).await
    }
    async fn canonical_path(&self, path: &str, c: &Context) -> FileResult<String> {
        self.inner.canonical_path(path, c).await
    }
    async fn exists(&self, path: &str, c: &Context) -> FileResult<bool> {
        self.inner.exists(path, c).await
    }
    async fn create_dir(&self, path: &str, recursive: bool, c: &Context) -> FileResult<()> {
        self.inner.create_dir(path, recursive, c).await
    }
    async fn remove(&self, path: &str, options: RemoveOptions, c: &Context) -> FileResult<()> {
        self.inner.remove(path, options, c).await
    }
    async fn create_temp_dir(&self, prefix: Option<&str>, c: &Context) -> FileResult<String> {
        if let Some(hook) = &self.before_temp_dir {
            hook().await;
        }
        self.inner.create_temp_dir(prefix, c).await
    }
    async fn create_temp_file(&self, options: TempFileOptions, c: &Context) -> FileResult<String> {
        self.inner.create_temp_file(options, c).await
    }
    async fn cleanup(&self, c: &Context) {
        self.inner.cleanup(c).await
    }
}

#[async_trait]
impl Shell for HookedEnv {
    async fn exec(
        &self,
        command: ShellCommand,
        options: ShellExecOptions,
        c: &Context,
    ) -> Result<ShellExecResult, ExecutionError> {
        self.inner.exec(command, options, c).await
    }
}
