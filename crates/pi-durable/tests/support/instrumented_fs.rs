//! A local file system that records mutating calls and fails one chosen call (upstream `InstrumentedEnv`).

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use pi_chord::Context;
use pi_durable::env::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Append,
    Flush,
    Write,
    Rename,
    Remove,
}

/// `Before` fails without effect, `After` fails after the effect, `Short` writes half of the bytes and fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Before,
    After,
    Short,
}

#[derive(Clone, Copy, Debug)]
pub struct Failure {
    pub operation: Operation,
    pub call: usize,
    pub mode: Mode,
}

#[derive(Default)]
struct Observations {
    failure: Option<Failure>,
    calls: [usize; 5],
    operations: Vec<String>,
}

pub struct InstrumentedFs {
    inner: LocalExecutionEnv,
    state: Mutex<Observations>,
}

fn base(path: &str) -> String {
    Path::new(path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

fn injected(message: &str, path: &str) -> FileError {
    FileError::new(FileErrorCode::Unknown, message, Some(path.to_string()))
}

impl InstrumentedFs {
    pub fn new(cwd: &str) -> Self {
        Self { inner: LocalExecutionEnv::new(LocalEnvOptions::new(cwd)), state: Mutex::new(Observations::default()) }
    }

    pub fn fail(&self, failure: Failure) {
        *self.state.lock().unwrap() = Observations { failure: Some(failure), ..Default::default() };
    }

    pub fn clear(&self) {
        *self.state.lock().unwrap() = Observations::default();
    }

    pub fn operations(&self) -> Vec<String> {
        self.state.lock().unwrap().operations.clone()
    }

    /// Record a call; returns the failure mode when this call is the one to fail.
    fn observe(&self, operation: Operation, description: String) -> Option<Mode> {
        let mut state = self.state.lock().unwrap();
        state.calls[operation as usize] += 1;
        state.operations.push(description);
        let call = state.calls[operation as usize];
        state
            .failure
            .filter(|failure| failure.operation == operation && failure.call == call)
            .map(|failure| failure.mode)
    }

    async fn partial<F, Fut>(
        &self,
        path: &str,
        content: &[u8],
        mode: Option<Mode>,
        write: F,
        name: &str,
    ) -> FileResult<()>
    where
        F: Fn(Vec<u8>) -> Fut,
        Fut: std::future::Future<Output = FileResult<()>>,
    {
        match mode {
            None => write(content.to_vec()).await,
            Some(Mode::Before) => Err(injected(&format!("injected {name} failure"), path)),
            Some(Mode::Short) => {
                write(content[..(content.len() / 2).max(1)].to_vec()).await?;
                Err(injected(&format!("injected short {name}"), path))
            }
            Some(Mode::After) => {
                write(content.to_vec()).await?;
                Err(injected(&format!("injected post-{name} failure"), path))
            }
        }
    }
}

#[async_trait]
impl FileSystem for InstrumentedFs {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn cwd(&self) -> String {
        self.inner.cwd()
    }
    async fn absolute_path(&self, path: &str, context: &Context) -> FileResult<String> {
        self.inner.absolute_path(path, context).await
    }
    async fn join_path(&self, parts: &[String], context: &Context) -> FileResult<String> {
        self.inner.join_path(parts, context).await
    }
    async fn read_text_file(&self, path: &str, context: &Context) -> FileResult<String> {
        self.inner.read_text_file(path, context).await
    }
    async fn open_text_line_reader(&self, path: &str, context: &Context) -> FileResult<Box<dyn TextLineReader>> {
        self.inner.open_text_line_reader(path, context).await
    }
    async fn read_text_lines(&self, path: &str, max: Option<usize>, context: &Context) -> FileResult<Vec<String>> {
        self.inner.read_text_lines(path, max, context).await
    }
    async fn read_binary_file(&self, path: &str, context: &Context) -> FileResult<Vec<u8>> {
        self.inner.read_binary_file(path, context).await
    }
    async fn open_binary_reader(
        &self,
        path: &str,
        no_follow: bool,
        context: &Context,
    ) -> FileResult<Box<dyn BinaryReader>> {
        self.inner.open_binary_reader(path, no_follow, context).await
    }
    async fn write_file(&self, path: &str, content: &[u8], context: &Context) -> FileResult<()> {
        let mode = self.observe(Operation::Write, format!("write:{}", base(path)));
        self.partial(
            path,
            content,
            mode,
            |bytes| async move { self.inner.write_file(path, &bytes, context).await },
            "write",
        )
        .await
    }
    async fn append_file(&self, path: &str, content: &[u8], context: &Context) -> FileResult<()> {
        let mode = self.observe(Operation::Append, format!("append:{}", base(path)));
        let append = |bytes: Vec<u8>| async move { self.inner.append_file(path, &bytes, context).await };
        self.partial(path, content, mode, append, "append").await
    }
    async fn truncate_file(&self, path: &str, size: u64, context: &Context) -> FileResult<()> {
        self.inner.truncate_file(path, size, context).await
    }
    async fn flush_file(&self, path: &str, context: &Context) -> FileResult<()> {
        match self.observe(Operation::Flush, format!("flush:{}", base(path))) {
            None => self.inner.flush_file(path, context).await,
            Some(Mode::After) => {
                self.inner.flush_file(path, context).await?;
                Err(injected("injected flush failure", path))
            }
            Some(_) => Err(injected("injected flush failure", path)),
        }
    }
    async fn rename_file(&self, source: &str, destination: &str, context: &Context) -> FileResult<()> {
        let description = format!("rename:{}->{}", base(source), base(destination));
        match self.observe(Operation::Rename, description) {
            None => self.inner.rename_file(source, destination, context).await,
            Some(Mode::After) => {
                self.inner.rename_file(source, destination, context).await?;
                Err(injected("injected rename failure", source))
            }
            Some(_) => Err(injected("injected rename failure", source)),
        }
    }
    async fn file_info(&self, path: &str, context: &Context) -> FileResult<FileInfo> {
        self.inner.file_info(path, context).await
    }
    async fn list_dir(&self, path: &str, context: &Context) -> FileResult<Vec<FileInfo>> {
        self.inner.list_dir(path, context).await
    }
    async fn open_dir_reader(&self, path: &str, context: &Context) -> FileResult<Box<dyn DirReader>> {
        self.inner.open_dir_reader(path, context).await
    }
    async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: WatchListener,
        context: &Context,
    ) -> FileResult<Box<dyn FileWatcher>> {
        self.inner.watch(targets, on_change, context).await
    }
    async fn canonical_path(&self, path: &str, context: &Context) -> FileResult<String> {
        self.inner.canonical_path(path, context).await
    }
    async fn exists(&self, path: &str, context: &Context) -> FileResult<bool> {
        self.inner.exists(path, context).await
    }
    async fn create_dir(&self, path: &str, recursive: bool, context: &Context) -> FileResult<()> {
        self.inner.create_dir(path, recursive, context).await
    }
    async fn remove(&self, path: &str, options: RemoveOptions, context: &Context) -> FileResult<()> {
        match self.observe(Operation::Remove, format!("remove:{}", base(path))) {
            None => self.inner.remove(path, options, context).await,
            Some(Mode::After) => {
                self.inner.remove(path, options, context).await?;
                Err(injected("injected remove failure", path))
            }
            Some(_) => Err(injected("injected remove failure", path)),
        }
    }
    async fn create_temp_dir(&self, prefix: Option<&str>, context: &Context) -> FileResult<String> {
        self.inner.create_temp_dir(prefix, context).await
    }
    async fn create_temp_file(&self, options: TempFileOptions, context: &Context) -> FileResult<String> {
        self.inner.create_temp_file(options, context).await
    }
    async fn cleanup(&self, context: &Context) {
        FileSystem::cleanup(&self.inner, context).await
    }
}
