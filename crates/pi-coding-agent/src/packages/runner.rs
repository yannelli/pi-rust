//! Running `npm`, `git`, and the `npmCommand` setting. The package manager only shells out
//! through [`CommandRunner`], so tests replace it with a mock and need neither npm nor git.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;

use crate::settings::SettingsManager;

/// One external command: program, arguments, working directory, extra environment, and an
/// optional timeout (used for captured commands).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub timeout: Option<Duration>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { program: program.into(), args: args.into_iter().map(Into::into).collect(), ..Self::default() }
    }

    pub fn in_dir(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_env(mut self, name: &str, value: &str) -> Self {
        self.env.push((name.to_string(), value.to_string()));
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// `program arg1 arg2`, as upstream error messages show commands.
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str()).chain(self.args.iter().map(String::as_str)).collect::<Vec<_>>().join(" ")
    }
}

#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run with inherited output; an error unless the command exits with 0.
    async fn run(&self, spec: &CommandSpec) -> anyhow::Result<()>;

    /// Run with captured output and return the trimmed stdout; an error (with stderr)
    /// unless the command exits with 0 before its timeout.
    async fn capture(&self, spec: &CommandSpec) -> anyhow::Result<String>;
}

/// Runs real processes. With `stdout_to_stderr`, child output goes to stderr so it does not
/// mix with JSON or print-mode output on stdout.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessRunner {
    pub stdout_to_stderr: bool,
}

/// Find `npm.cmd` and other `PATHEXT` programs, which Windows process creation does not search.
#[cfg(windows)]
fn resolve_program(program: &str) -> PathBuf {
    let path = std::path::Path::new(program);
    if path.extension().is_some() || program.contains(['/', '\\']) {
        return path.to_path_buf();
    }
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let dirs =
        std::env::var_os("PATH").map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>()).unwrap_or_default();
    for dir in dirs {
        for extension in extensions.split(';').filter(|extension| !extension.is_empty()) {
            let candidate = dir.join(format!("{program}{}", extension.to_ascii_lowercase()));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    path.to_path_buf()
}

#[cfg(not(windows))]
fn resolve_program(program: &str) -> PathBuf {
    PathBuf::from(program)
}

impl ProcessRunner {
    fn command(&self, spec: &CommandSpec) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(resolve_program(&spec.program));
        command.args(&spec.args).envs(pi_ai::env::child_process_env()).kill_on_drop(true);
        command.envs(spec.env.iter().map(|(name, value)| (name.as_str(), value.as_str())));
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        command
    }
}

fn exit_code_text(status: std::process::ExitStatus) -> String {
    status.code().map_or_else(|| "null".to_string(), |code| code.to_string())
}

#[async_trait]
impl CommandRunner for ProcessRunner {
    async fn run(&self, spec: &CommandSpec) -> anyhow::Result<()> {
        let mut command = self.command(spec);
        command.stdin(Stdio::inherit()).stderr(Stdio::inherit());
        command.stdout(if self.stdout_to_stderr { Stdio::from(std::io::stderr()) } else { Stdio::inherit() });
        let status = command.status().await.with_context(|| format!("spawn {}", spec.program))?;
        if !status.success() {
            bail!("{} failed with code {}", spec.display(), exit_code_text(status));
        }
        Ok(())
    }

    async fn capture(&self, spec: &CommandSpec) -> anyhow::Result<String> {
        let mut command = self.command(spec);
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = command.spawn().with_context(|| format!("spawn {}", spec.program))?;
        let output = match spec.timeout {
            Some(timeout) => tokio::time::timeout(timeout, child.wait_with_output())
                .await
                .map_err(|_| anyhow!("{} timed out after {}ms", spec.display(), timeout.as_millis()))??,
            None => child.wait_with_output().await?,
        };
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if output.status.success() {
            return Ok(stdout.trim().to_string());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = if stderr.is_empty() { stdout.as_str() } else { &stderr };
        bail!("{} failed with code {}: {detail}", spec.display(), exit_code_text(output.status))
    }
}

/// The `npmCommand` setting split into program and leading arguments (default `npm`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NpmCommand {
    pub command: String,
    pub args: Vec<String>,
}

impl NpmCommand {
    pub fn from_settings(settings: &SettingsManager) -> anyhow::Result<Self> {
        Self::from_argv(settings.npm_command().as_deref())
    }

    pub fn from_argv(argv: Option<&[String]>) -> anyhow::Result<Self> {
        let Some((command, args)) = argv.and_then(<[String]>::split_first) else {
            return Ok(Self { command: "npm".into(), args: Vec::new() });
        };
        if command.is_empty() {
            bail!("Invalid npmCommand: first array entry must be a non-empty command");
        }
        Ok(Self { command: command.clone(), args: args.to_vec() })
    }

    pub fn spec(&self, args: impl IntoIterator<Item = impl Into<String>>) -> CommandSpec {
        let all = self.args.iter().cloned().chain(args.into_iter().map(Into::into));
        CommandSpec::new(self.command.clone(), all.collect::<Vec<String>>())
    }

    /// Joined argv, used to cache lookups per configured command.
    pub fn key(&self) -> String {
        std::iter::once(&self.command).chain(&self.args).cloned().collect::<Vec<_>>().join("\0")
    }

    /// `npm`, `pnpm`, or `bun`, looking through wrappers such as `mise exec -- pnpm` and
    /// `corepack pnpm`; otherwise the command name.
    pub fn package_manager_name(&self) -> anyhow::Result<String> {
        let normalize = |command: &str| {
            let name = std::path::Path::new(command).file_name().map(|name| name.to_string_lossy().into_owned());
            let name = name.unwrap_or_else(|| command.to_string());
            let lower = name.to_ascii_lowercase();
            if lower.ends_with(".cmd") || lower.ends_with(".exe") { name[..name.len() - 4].to_string() } else { name }
        };
        let supported = ["npm", "pnpm", "bun"];
        let direct = normalize(&self.command);
        if let Some(separator) = self.args.iter().rposition(|arg| arg == "--") {
            return Ok(self.args.get(separator + 1).map_or(direct, |wrapped| normalize(wrapped)));
        }
        if supported.contains(&direct.as_str()) {
            return Ok(direct);
        }
        let mut wrapped: Vec<String> = Vec::new();
        for name in self.args.iter().map(|arg| normalize(arg)) {
            if supported.contains(&name.as_str()) && !wrapped.contains(&name) {
                wrapped.push(name);
            }
        }
        if wrapped.len() > 1 {
            bail!("Ambiguous npmCommand package managers: {}", wrapped.join(", "));
        }
        Ok(wrapped.pop().unwrap_or(direct))
    }
}

/// Install arguments for managed npm packages. Peer installation is disabled so package
/// managers never install or solve the host-provided `@earendil-works/pi-*` peers.
pub fn npm_install_args(manager: &str, specs: &[String], root: &std::path::Path) -> Vec<String> {
    let root = root.display().to_string();
    let mut args = vec!["install".to_string()];
    args.extend(specs.iter().cloned());
    let tail: Vec<&str> = match manager {
        "bun" => vec!["--cwd", &root, "--omit=peer"],
        "pnpm" => vec![
            "--prefix",
            &root,
            "--config.auto-install-peers=false",
            "--config.strict-peer-dependencies=false",
            "--config.strict-dep-builds=false",
        ],
        _ => vec!["--prefix", &root, "--legacy-peer-deps"],
    };
    args.extend(tail.into_iter().map(String::from));
    args
}

/// Dependency install arguments for a git or archive package checkout.
pub fn git_dependency_install_args(manager: &str) -> Vec<String> {
    let args: &[&str] = match manager {
        "bun" => &["install", "--omit=dev", "--omit=peer"],
        "pnpm" => &[
            "install",
            "--prod",
            "--config.auto-install-peers=false",
            "--config.strict-peer-dependencies=false",
            "--config.strict-dep-builds=false",
        ],
        "npm" => &["install", "--omit=dev", "--legacy-peer-deps"],
        _ => &["install"],
    };
    args.iter().map(|arg| arg.to_string()).collect()
}

pub fn npm_uninstall_args(manager: &str, name: &str, root: &std::path::Path) -> Vec<String> {
    let root = root.display().to_string();
    match manager {
        "bun" => vec!["uninstall".into(), name.into(), "--cwd".into(), root],
        "pnpm" => vec!["uninstall".into(), name.into(), "--prefix".into(), root],
        _ => vec!["uninstall".into(), name.into(), "--prefix".into(), root, "--legacy-peer-deps".into()],
    }
}
