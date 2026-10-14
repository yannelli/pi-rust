//! Helpers of the coding tool tests: a call api without a Harness (upstream `fakeApi`) and a local environment whose
//! writes, reads, readers, and commands a test can intercept (upstream subclasses of `NodeExecutionEnv`).
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use pi_ai::{JsonObject, Model};
use pi_chord::{BACKGROUND_CONTEXT, Context};
use pi_durable::env::*;
use pi_durable::harness::{ToolDiagnostic, ToolExecutionResult};
use pi_durable::tools::{ImageModel, ToolCallApi, image_model_of};
use serde_json::Value;

pub fn ctx() -> Context {
    BACKGROUND_CONTEXT.clone()
}

pub fn args(value: Value) -> JsonObject {
    value.as_object().cloned().expect("arguments are an object")
}

/// A call api that collects output and diagnostics; `model` is the conversation's model, none by default.
#[derive(Default)]
pub struct FakeApi {
    pub env: Option<SharedEnv>,
    pub window: Option<ShellOutputWindow>,
    pub model: Option<Model>,
    pub output: Mutex<Vec<(String, Option<ShellOutputSkip>)>>,
    pub diagnostics: Mutex<Vec<ToolDiagnostic>>,
}

impl FakeApi {
    pub fn new(env: Option<SharedEnv>) -> Arc<Self> {
        Arc::new(Self { env, ..Self::default() })
    }

    pub fn streamed(&self) -> String {
        self.output.lock().unwrap().iter().map(|(text, _)| text.as_str()).collect()
    }

    pub fn reported(&self) -> Vec<ToolDiagnostic> {
        self.diagnostics.lock().unwrap().clone()
    }
}

#[async_trait]
impl ToolCallApi for FakeApi {
    fn env(&self) -> Option<SharedEnv> {
        self.env.clone()
    }

    fn output_window(&self) -> Option<ShellOutputWindow> {
        self.window
    }

    fn output(&self, chunk: &str, skipped: Option<ShellOutputSkip>) -> pi_durable::DurableResult<()> {
        self.output.lock().unwrap().push((chunk.to_string(), skipped));
        Ok(())
    }

    fn retained_output(&self) -> pi_durable::DurableResult<(String, bool)> {
        Ok((self.streamed(), false))
    }

    fn diagnostic(&self, diagnostic: ToolDiagnostic) -> pi_durable::DurableResult<()> {
        self.diagnostics.lock().unwrap().push(diagnostic);
        Ok(())
    }

    async fn image_model(&self, _context: &Context) -> pi_durable::DurableResult<ImageModel> {
        Ok(image_model_of(self.model.as_ref()))
    }
}

pub fn text_output(result: &ToolExecutionResult) -> String {
    let output = result.output.as_deref().unwrap_or_default();
    output.iter().filter_map(|block| block.as_text()).collect::<Vec<_>>().join("\n")
}

pub fn messages(result: &ToolExecutionResult) -> Vec<String> {
    result.diagnostics.iter().flatten().map(|diagnostic| diagnostic.message.clone()).collect()
}

/// Runs before a write with its content; `true` makes the write ignore the call's cancellation.
pub type WriteHook = Arc<dyn Fn(Vec<u8>) -> BoxFuture<'static, bool> + Send + Sync>;
pub type ReaderHook = Arc<dyn Fn(Box<dyn BinaryReader>) -> Box<dyn BinaryReader> + Send + Sync>;
pub type ExecHook = Arc<
    dyn Fn(
            Arc<LocalExecutionEnv>,
            ShellCommand,
            ShellExecOptions,
            Context,
        ) -> BoxFuture<'static, Result<ShellExecResult, ExecutionError>>
        + Send
        + Sync,
>;

/// A local environment with optional interceptions.
pub struct HookedEnv {
    pub inner: Arc<LocalExecutionEnv>,
    pub id: Option<String>,
    pub read_delay: Option<Duration>,
    pub on_write: Option<WriteHook>,
    pub on_reader: Option<ReaderHook>,
    pub on_exec: Option<ExecHook>,
}

impl HookedEnv {
    pub fn new(cwd: &str) -> Self {
        Self::wrap(LocalExecutionEnv::new(LocalEnvOptions::new(cwd)))
    }

    pub fn wrap(inner: LocalExecutionEnv) -> Self {
        let inner = Arc::new(inner);
        Self { inner, id: None, read_delay: None, on_write: None, on_reader: None, on_exec: None }
    }
}

#[async_trait]
impl FileSystem for HookedEnv {
    fn id(&self) -> &str {
        self.id.as_deref().unwrap_or_else(|| self.inner.id())
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
        if let Some(delay) = self.read_delay {
            tokio::time::sleep(delay).await;
        }
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
        let reader = self.inner.open_binary_reader(path, no_follow, c).await?;
        Ok(match &self.on_reader {
            Some(hook) => hook(reader),
            None => reader,
        })
    }
    async fn write_file(&self, path: &str, content: &[u8], c: &Context) -> FileResult<()> {
        let detached = match &self.on_write {
            Some(hook) => hook(content.to_vec()).await,
            None => false,
        };
        self.inner.write_file(path, content, if detached { &BACKGROUND_CONTEXT } else { c }).await
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
        match &self.on_exec {
            Some(hook) => hook(self.inner.clone(), command, options, c.clone()).await,
            None => self.inner.exec(command, options, c).await,
        }
    }
}

/// A binary reader that runs `on_read(offset, length)` after each positional read.
pub struct ObservedReader {
    pub inner: Box<dyn BinaryReader>,
    pub on_read: Arc<dyn Fn(u64, u64) + Send + Sync>,
}

#[async_trait]
impl BinaryReader for ObservedReader {
    async fn info(&self, c: &Context) -> FileResult<FileInfo> {
        self.inner.info(c).await
    }
    async fn read(&self, offset: u64, length: u64, c: &Context) -> FileResult<Vec<u8>> {
        let bytes = self.inner.read(offset, length, c).await?;
        (self.on_read)(offset, length);
        Ok(bytes)
    }
    async fn scan_lines(&self, range: LineRange, c: &Context) -> FileResult<LineScan> {
        self.inner.scan_lines(range, c).await
    }
    async fn close(&self, c: &Context) {
        self.inner.close(c).await
    }
}
