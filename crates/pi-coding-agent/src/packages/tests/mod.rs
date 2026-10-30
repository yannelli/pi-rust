//! Tests ported from upstream `package-manager.test.ts`, `git-update.test.ts`,
//! `git-ssh-url.test.ts`, and `package-command-paths.test.ts`, plus URL archive tests.
//! npm runs through a mock runner; git tests use a local repository and skip without git.

mod archive;
mod cli;
mod config;
mod git;
mod install;
mod native;
mod resolve;
mod sources;
mod update;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::runner::{CommandRunner, CommandSpec, ProcessRunner};
use super::{PackageManager, PackageManagerOptions};
use crate::settings::{SettingsManager, SettingsOptions};

/// Package entries from JSON, as settings store them.
pub fn sources(value: serde_json::Value) -> Vec<crate::settings::schema::PackageSource> {
    serde_json::from_value(value).unwrap()
}

pub fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

pub const EXTENSION: &str = "export default function() {}";

pub fn skill(name: &str) -> String {
    format!("---\nname: {name}\ndescription: The {name} skill\n---\n\n# {name}\n")
}

/// Forward-slash form of a path, for suffix checks on every platform.
pub fn posix(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallKind {
    Run,
    Capture,
}

#[derive(Clone, Debug)]
pub struct Call {
    pub kind: CallKind,
    pub spec: CommandSpec,
}

impl Call {
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.spec.program.clone()).chain(self.spec.args.iter().cloned()).collect()
    }
}

type Handler = Box<dyn Fn(&Call) -> anyhow::Result<String> + Send + Sync>;

/// Records every command; `handler` answers them. With `git_env`, git commands run for real
/// with those extra variables (used to map `https://github.com/...` to a local repository).
pub struct MockRunner {
    pub calls: Mutex<Vec<Call>>,
    handler: Handler,
    git_env: Option<Vec<(String, String)>>,
    /// With a delay, each command waits and the peak number running per program is kept.
    delay: Option<std::time::Duration>,
    active: Mutex<std::collections::HashMap<String, (usize, usize)>>,
}

impl MockRunner {
    pub fn new(handler: impl Fn(&Call) -> anyhow::Result<String> + Send + Sync + 'static) -> Arc<Self> {
        Self::build(handler, None, None)
    }

    fn build(
        handler: impl Fn(&Call) -> anyhow::Result<String> + Send + Sync + 'static,
        git_env: Option<Vec<(String, String)>>,
        delay: Option<std::time::Duration>,
    ) -> Arc<Self> {
        let calls = Mutex::new(Vec::new());
        Arc::new(Self { calls, handler: Box::new(handler), git_env, delay, active: Mutex::default() })
    }

    pub fn delayed(
        delay: std::time::Duration,
        handler: impl Fn(&Call) -> anyhow::Result<String> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::build(handler, None, Some(delay))
    }

    /// The most commands of `program` that ran at once.
    pub fn peak(&self, program: &str) -> usize {
        self.active.lock().unwrap().get(program).map_or(0, |(_, peak)| *peak)
    }

    /// Fails every command, like a machine without npm.
    pub fn failing() -> Arc<Self> {
        Self::new(|call| anyhow::bail!("unexpected command: {}", call.argv().join(" ")))
    }

    pub fn with_real_git(
        git_env: Vec<(String, String)>,
        handler: impl Fn(&Call) -> anyhow::Result<String> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::build(handler, Some(git_env), None)
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    pub fn argvs(&self, kind: CallKind) -> Vec<Vec<String>> {
        self.calls().into_iter().filter(|call| call.kind == kind).map(|call| call.argv()).collect()
    }

    async fn dispatch(&self, kind: CallKind, spec: &CommandSpec) -> anyhow::Result<String> {
        self.calls.lock().unwrap().push(Call { kind, spec: spec.clone() });
        if let Some(env) = self.git_env.as_ref().filter(|_| spec.program == "git") {
            let mut spec = spec.clone();
            spec.env.extend(env.iter().cloned());
            let output = ProcessRunner::default().capture(&spec).await;
            return if kind == CallKind::Run { output.map(|_| String::new()) } else { output };
        }
        if let Some(delay) = self.delay {
            {
                let mut active = self.active.lock().unwrap();
                let entry = active.entry(spec.program.clone()).or_default();
                entry.0 += 1;
                entry.1 = entry.1.max(entry.0);
            }
            tokio::time::sleep(delay).await;
            self.active.lock().unwrap().entry(spec.program.clone()).or_default().0 -= 1;
        }
        (self.handler)(&Call { kind, spec: spec.clone() })
    }
}

#[async_trait]
impl CommandRunner for MockRunner {
    async fn run(&self, spec: &CommandSpec) -> anyhow::Result<()> {
        self.dispatch(CallKind::Run, spec).await.map(|_| ())
    }

    async fn capture(&self, spec: &CommandSpec) -> anyhow::Result<String> {
        self.dispatch(CallKind::Capture, spec).await
    }
}

/// A project directory and an agent directory in one temp dir, with file-backed settings.
pub struct Fixture {
    _temp: tempfile::TempDir,
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub settings: Arc<SettingsManager>,
}

impl Fixture {
    pub fn new() -> Self {
        Self::with_trust(true)
    }

    pub fn with_trust(trusted: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::resources::paths::canonicalize(temp.path());
        let cwd = root.join("project");
        let agent_dir = root.join("agent");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        let settings =
            Arc::new(SettingsManager::create(&cwd, &agent_dir, SettingsOptions { project_trusted: trusted }));
        Self { _temp: temp, cwd, agent_dir, settings }
    }

    pub fn root(&self) -> PathBuf {
        self.cwd.parent().unwrap().to_path_buf()
    }

    pub fn set_global(&self, settings: serde_json::Value) {
        write(&self.agent_dir.join("settings.json"), &settings.to_string());
        self.settings.reload();
    }

    pub fn manager(&self, runner: Arc<dyn CommandRunner>) -> PackageManager {
        let mut options = PackageManagerOptions::new(&self.cwd, &self.agent_dir, self.settings.clone());
        options.home = self.root();
        options.runner = runner;
        options.offline = Some(false);
        options.http_client = Some(reqwest::Client::builder().no_proxy().build().unwrap());
        PackageManager::new(options)
    }

    pub fn offline_manager(&self, runner: Arc<dyn CommandRunner>) -> PackageManager {
        let mut options = PackageManagerOptions::new(&self.cwd, &self.agent_dir, self.settings.clone());
        options.home = self.root();
        options.runner = runner;
        options.offline = Some(true);
        PackageManager::new(options)
    }

    pub fn global_packages(&self) -> serde_json::Value {
        serde_json::to_value(self.settings.global_settings().packages.unwrap_or_default()).unwrap()
    }

    pub fn project_packages(&self) -> serde_json::Value {
        serde_json::to_value(self.settings.project_settings().packages.unwrap_or_default()).unwrap()
    }
}

/// Write `node_modules/<name>/package.json` (and optional files) under an npm install root.
pub fn fake_npm_package(root: &Path, name: &str, version: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = root.join("node_modules").join(name);
    write(&dir.join("package.json"), &serde_json::json!({ "name": name, "version": version }).to_string());
    for (path, content) in files {
        write(&dir.join(path), content);
    }
    dir
}

/// The `--prefix` (or bun `--cwd`) value of an npm install call.
pub fn install_root(argv: &[String]) -> Option<PathBuf> {
    let index = argv.iter().position(|arg| arg == "--prefix" || arg == "--cwd")?;
    argv.get(index + 1).map(PathBuf::from)
}

pub fn git_available() -> bool {
    std::process::Command::new("git").arg("--version").output().is_ok_and(|output| output.status.success())
}
