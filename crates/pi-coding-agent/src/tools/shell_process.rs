//! Local process execution for the shell tools: shell resolution, streaming, and killing the
//! process tree. Port of upstream `utils/shell.ts` and `createLocalShellOperations`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;
/// How long to keep reading after the shell exits while descendants still hold the pipes.
const POST_EXIT_IDLE: Duration = Duration::from_millis(250);
/// Arguments the PowerShell tool runs commands with.
pub const POWERSHELL_ARGS: [&str; 5] = ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"];
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Receives raw output chunks, stdout and stderr interleaved.
pub type ShellDataCallback = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// How the command reaches the shell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CommandTransport {
    /// As the last argument.
    #[default]
    Argv,
    /// On standard input, for shells such as the legacy WSL `bash.exe`.
    Stdin,
}

/// A shell program and the arguments that come before the command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellConfig {
    pub shell: PathBuf,
    pub args: Vec<String>,
    pub transport: CommandTransport,
}

/// Options of one [`ShellOperations::exec`] call.
pub struct ShellExecOptions {
    pub on_data: ShellDataCallback,
    pub cancel: CancellationToken,
    /// Timeout in seconds, as the model gave it.
    pub timeout: Option<f64>,
    /// The full environment of the command; `None` inherits this process's.
    pub env: Option<BTreeMap<String, String>>,
}

/// Why a command did not run to completion.
#[derive(Debug)]
pub enum ShellExecError {
    Aborted,
    /// Timed out after the given seconds.
    TimedOut(f64),
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for ShellExecError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

/// Runs shell commands for the shell tools. Implement it to run commands elsewhere, for
/// example over SSH (upstream `BashOperations`).
#[async_trait]
pub trait ShellOperations: Send + Sync {
    /// Run `command` in `cwd` and return its exit code. Report signal terminations as
    /// 128 + signal; `None` counts as a failed command.
    async fn exec(&self, command: &str, cwd: &Path, options: ShellExecOptions) -> Result<Option<i32>, ShellExecError>;
}

type ShellResolver = Arc<dyn Fn() -> anyhow::Result<ShellConfig> + Send + Sync>;

/// Runs commands as local processes, each in its own process group.
#[derive(Clone)]
pub struct LocalShellOperations {
    shell_name: String,
    resolve: ShellResolver,
}

impl LocalShellOperations {
    /// `shell_name` appears in errors; `resolve` picks the shell before each command.
    pub fn new(
        shell_name: impl Into<String>,
        resolve: impl Fn() -> anyhow::Result<ShellConfig> + Send + Sync + 'static,
    ) -> Self {
        Self { shell_name: shell_name.into(), resolve: Arc::new(resolve) }
    }

    /// Bash, or the `shellPath` setting when given (upstream `createLocalBashOperations`).
    pub fn bash(shell_path: Option<PathBuf>) -> Self {
        Self::new("bash", move || bash_shell_config(shell_path.as_deref()))
    }
}

pub(crate) fn resolve_timeout(timeout: Option<f64>) -> anyhow::Result<Option<Duration>> {
    let Some(secs) = timeout else { return Ok(None) };
    if !secs.is_finite() || secs <= 0.0 {
        bail!("Invalid timeout: must be a finite number of seconds");
    }
    if secs * 1000.0 > MAX_TIMEOUT_MS {
        bail!("Invalid timeout: maximum is {} seconds", MAX_TIMEOUT_MS / 1000.0);
    }
    Ok(Some(Duration::from_secs_f64(secs)))
}

fn is_legacy_wsl_bash_path(path: &Path) -> bool {
    let normalized = path.to_string_lossy().replace('/', "\\").to_lowercase();
    let bytes = normalized.as_bytes();
    bytes.len() > 3
        && bytes[0].is_ascii_lowercase()
        && normalized[1..].starts_with(":\\windows\\")
        && matches!(&normalized[11..], "system32\\bash.exe" | "sysnative\\bash.exe")
}

fn bash_config_for(shell: PathBuf) -> ShellConfig {
    if is_legacy_wsl_bash_path(&shell) {
        return ShellConfig { shell, args: vec!["-s".into()], transport: CommandTransport::Stdin };
    }
    ShellConfig { shell, args: vec!["-c".into()], transport: CommandTransport::Argv }
}

pub(crate) fn find_executable_on_path(executable: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|dir| dir.join(executable)).find(|candidate| candidate.is_file())
}

/// Bash for the bash tool: `custom` when given, then Git Bash and `bash.exe` on PATH on
/// Windows, or `/bin/bash`, `bash` on PATH, and `sh` elsewhere.
pub fn bash_shell_config(custom: Option<&Path>) -> anyhow::Result<ShellConfig> {
    if let Some(path) = custom {
        if path.exists() {
            return Ok(bash_config_for(path.to_path_buf()));
        }
        bail!("Custom shell path not found: {}", path.display());
    }
    if cfg!(windows) {
        let candidates: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)"]
            .iter()
            .filter_map(std::env::var_os)
            .map(|dir| PathBuf::from(dir).join("Git").join("bin").join("bash.exe"))
            .collect();
        if let Some(found) = candidates.iter().find(|path| path.exists()) {
            return Ok(bash_config_for(found.clone()));
        }
        if let Some(found) = find_executable_on_path("bash.exe") {
            return Ok(bash_config_for(found));
        }
        let searched: Vec<String> = candidates.iter().map(|path| format!("  {}", path.display())).collect();
        bail!(
            "No bash shell found. Options:\n  1. Install Git for Windows: https://git-scm.com/download/win\n  2. Add your bash to PATH (Cygwin, MSYS2, etc.)\n  3. Set shellPath in settings.json\n\nSearched Git Bash in:\n{}",
            searched.join("\n")
        );
    }
    Ok(bash_config_for(default_unix_shell()))
}

fn default_unix_shell() -> PathBuf {
    if Path::new("/bin/bash").exists() {
        return PathBuf::from("/bin/bash");
    }
    find_executable_on_path("bash").unwrap_or_else(|| PathBuf::from("sh"))
}

/// PowerShell on Windows, preferring PowerShell 7 (`pwsh.exe`).
pub fn powershell_shell_config() -> anyhow::Result<ShellConfig> {
    if !cfg!(windows) {
        bail!("The powershell tool is only available on Windows.");
    }
    let Some(shell) = find_executable_on_path("pwsh.exe").or_else(|| find_executable_on_path("powershell.exe")) else {
        bail!("No PowerShell executable found. Install PowerShell or add powershell.exe/pwsh.exe to PATH.");
    };
    let args = POWERSHELL_ARGS.iter().map(|arg| arg.to_string()).collect();
    Ok(ShellConfig { shell, args, transport: CommandTransport::Argv })
}

/// This process's environment with the child-process overlay (upstream `getShellEnv`).
pub fn shell_env() -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    env.extend(pi_ai::env::child_process_env());
    env
}

fn spawn_reader(mut stream: impl AsyncRead + Unpin + Send + 'static, tx: mpsc::UnboundedSender<Vec<u8>>) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        while let Ok(n) = stream.read(&mut buf).await {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
}

/// Kill the child and its descendants: its process group on Unix, `taskkill /T` on Windows.
pub(crate) async fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        let mut command = kill_tree_command(pid);
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let _ = command.status().await;
    }
    let _ = child.start_kill();
}

#[cfg(windows)]
fn kill_tree_command(pid: u32) -> tokio::process::Command {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Windows"));
    let mut command = tokio::process::Command::new(root.join("System32").join("taskkill.exe"));
    command.args(["/F", "/T", "/PID", &pid.to_string()]).creation_flags(CREATE_NO_WINDOW);
    command
}

/// Uses the shell's `kill` builtin because a standalone `kill` binary is not always installed.
#[cfg(not(windows))]
fn kill_tree_command(pid: u32) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(default_unix_shell());
    command.args(["-c", &format!("kill -KILL -- -{pid}")]);
    command
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(1)
}

enum Outcome {
    Exited(i32),
    Aborted,
    TimedOut,
}

#[async_trait]
impl ShellOperations for LocalShellOperations {
    async fn exec(&self, command: &str, cwd: &Path, options: ShellExecOptions) -> Result<Option<i32>, ShellExecError> {
        let timeout = resolve_timeout(options.timeout)?;
        if options.cancel.is_cancelled() {
            return Err(ShellExecError::Aborted);
        }
        let config = (self.resolve)()?;
        if !cwd.exists() {
            let message = format!(
                "Working directory does not exist: {}\nCannot execute {} commands.",
                cwd.display(),
                self.shell_name
            );
            return Err(anyhow!(message).into());
        }

        let from_stdin = config.transport == CommandTransport::Stdin;
        let mut process = tokio::process::Command::new(&config.shell);
        process.args(&config.args);
        if !from_stdin {
            process.arg(command);
        }
        match options.env {
            Some(env) => process.env_clear().envs(env),
            None => process.envs(shell_env()),
        };
        process
            .current_dir(cwd)
            .stdin(if from_stdin { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        process.process_group(0);
        #[cfg(windows)]
        process.creation_flags(CREATE_NO_WINDOW);
        let mut child = process.spawn().map_err(|e| anyhow!("Failed to start {}: {e}", self.shell_name))?;
        if let Some(mut stdin) = child.stdin.take() {
            let script = command.as_bytes().to_vec();
            tokio::spawn(async move {
                let _ = stdin.write_all(&script).await;
                let _ = stdin.shutdown().await;
            });
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, tx);
        }

        let deadline = timeout.map(|t| Instant::now() + t);
        let far_future = Instant::now() + Duration::from_secs(86_400 * 365);
        let (mut status, mut streams_done, mut idle_deadline) = (None, false, far_future);
        let outcome = loop {
            if let (Some(code), true) = (status, streams_done) {
                break Outcome::Exited(code);
            }
            tokio::select! {
                _ = options.cancel.cancelled() => break Outcome::Aborted,
                _ = tokio::time::sleep_until(deadline.unwrap_or(far_future)) => break Outcome::TimedOut,
                chunk = rx.recv(), if !streams_done => match chunk {
                    Some(data) => {
                        (options.on_data)(&data);
                        if status.is_some() {
                            idle_deadline = Instant::now() + POST_EXIT_IDLE;
                        }
                    }
                    None => streams_done = true,
                },
                result = child.wait(), if status.is_none() => {
                    status = Some(exit_code(result.map_err(anyhow::Error::from)?));
                    idle_deadline = Instant::now() + POST_EXIT_IDLE;
                }
                _ = tokio::time::sleep_until(idle_deadline), if status.is_some() => streams_done = true,
            }
        };

        if let Outcome::Exited(code) = outcome {
            return Ok(Some(code));
        }
        kill_tree(&mut child).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        while let Ok(Some(data)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            (options.on_data)(&data);
        }
        Err(match outcome {
            Outcome::Aborted => ShellExecError::Aborted,
            _ => ShellExecError::TimedOut(options.timeout.unwrap_or_default()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_wsl_bash_reads_the_command_from_stdin() {
        for path in ["C:\\Windows\\System32\\bash.exe", "c:/windows/sysnative/bash.exe"] {
            let config = bash_config_for(PathBuf::from(path));
            assert_eq!((config.args, config.transport), (vec!["-s".to_string()], CommandTransport::Stdin));
        }
        let git = bash_config_for(PathBuf::from("C:\\Program Files\\Git\\bin\\bash.exe"));
        assert_eq!((git.args, git.transport), (vec!["-c".to_string()], CommandTransport::Argv));
    }

    #[test]
    fn resolves_custom_and_default_shells() {
        assert!(bash_shell_config(Some(Path::new("/no/such/shell"))).unwrap_err().to_string().contains("not found"));
        #[cfg(unix)]
        assert_eq!(bash_shell_config(None).unwrap().args, ["-c"]);
        #[cfg(not(windows))]
        assert_eq!(
            powershell_shell_config().unwrap_err().to_string(),
            "The powershell tool is only available on Windows."
        );
    }
}
