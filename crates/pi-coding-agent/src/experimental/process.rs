//! Pi-owned internal processes (upstream `experimental/process.ts`): roles, spawning, and control lines.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

use pi_chord::{ChordError, ChordResult};
use tokio::sync::watch;

pub const INTERNAL_PROCESS_ENV: &str = "__PI_INTERNAL_SPAWN";
pub const MAX_CONTROL_LINE_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InternalProcessRole {
    Coordinator,
    Server,
    SessionWorker,
}

impl InternalProcessRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coordinator => "coordinator",
            Self::Server => "server",
            Self::SessionWorker => "session-worker",
        }
    }
}

static CONSUMED: AtomicBool = AtomicBool::new(false);
static PROGRAMS: LazyLock<Mutex<HashMap<Option<InternalProcessRole>, PathBuf>>> = LazyLock::new(Default::default);

/// Read and validate an internal process role without consuming it. An empty value counts as unset.
pub fn get_internal_process_role() -> ChordResult<Option<InternalProcessRole>> {
    if CONSUMED.load(Ordering::SeqCst) {
        return Ok(None);
    }
    match std::env::var(INTERNAL_PROCESS_ENV).ok().as_deref() {
        None | Some("") => Ok(None),
        Some("coordinator") => Ok(Some(InternalProcessRole::Coordinator)),
        Some("server") => Ok(Some(InternalProcessRole::Server)),
        Some("session-worker") => Ok(Some(InternalProcessRole::SessionWorker)),
        Some(role) => Err(ChordError::error(format!("Unsupported internal process role: {role}"))),
    }
}

/// Read, validate, and hide the role. Rust cannot safely unset a variable, so descendants get it empty.
pub fn consume_internal_process_role() -> ChordResult<Option<InternalProcessRole>> {
    let role = get_internal_process_role()?;
    CONSUMED.store(true, Ordering::SeqCst);
    pi_ai::env::set_child_process_env(INTERNAL_PROCESS_ENV, "");
    Ok(role)
}

/// Select the executable for one role, or for every role with `None`. Defaults to the current executable.
pub fn set_internal_process_program(role: Option<InternalProcessRole>, program: Option<PathBuf>) {
    let mut programs = PROGRAMS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match program {
        Some(program) => programs.insert(role, program),
        None => programs.remove(&role),
    };
}

fn program_for(role: InternalProcessRole) -> ChordResult<PathBuf> {
    let programs = PROGRAMS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(program) = programs.get(&Some(role)).or_else(|| programs.get(&None)) {
        return Ok(program.clone());
    }
    std::env::current_exe().map_err(ChordError::other)
}

#[derive(Default)]
pub struct InternalProcessSpawnOptions {
    /// Overrides the role's executable, as upstream's `entryUrl`.
    pub program: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

/// A detached internal process. Dropping it never kills the process.
#[derive(Clone)]
pub struct InternalProcess {
    pid: u32,
    exit: watch::Receiver<Option<String>>,
}

impl InternalProcess {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The exit code or signal description once the process exited.
    pub fn exit_description(&self) -> Option<String> {
        self.exit.borrow().clone()
    }

    pub fn has_exited(&self) -> bool {
        self.exit.borrow().is_some()
    }

    /// Wait until the process exits and return its exit description.
    pub async fn exited(&self) -> String {
        let mut exit = self.exit.clone();
        match exit.wait_for(Option::is_some).await {
            Ok(value) => value.clone().unwrap_or_default(),
            Err(_) => "unknown".into(),
        }
    }

    pub fn kill(&self) {
        if !self.has_exited() {
            kill_pid(self.pid);
        }
    }
}

/// Spawn a detached Pi-owned process with the role in its environment.
pub fn spawn_internal_process(
    role: InternalProcessRole,
    args: &[String],
    options: InternalProcessSpawnOptions,
) -> ChordResult<InternalProcess> {
    let program = match options.program {
        Some(program) => program,
        None => program_for(role)?,
    };
    let mut command = tokio::process::Command::new(program);
    command.args(args).envs(pi_ai::env::child_process_env()).envs(options.env).env(INTERNAL_PROCESS_ENV, role.as_str());
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if let Ok(cwd) = std::env::current_dir() {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x0000_0008 | 0x0800_0000);
    let mut child = command.spawn().map_err(ChordError::other)?;
    let pid = child.id().ok_or_else(|| ChordError::error("Internal process exited during spawn"))?;
    let (sender, exit) = watch::channel(None);
    tokio::spawn(async move {
        let description = match child.wait().await {
            Ok(status) => describe_status(status),
            Err(_) => "unknown".to_string(),
        };
        let _ = sender.send(Some(description));
    });
    Ok(InternalProcess { pid, exit })
}

fn describe_status(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return signal_name(signal);
        }
    }
    status.code().map_or_else(|| "unknown".to_string(), |code| code.to_string())
}

#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    match signal {
        9 => "SIGKILL".into(),
        15 => "SIGTERM".into(),
        2 => "SIGINT".into(),
        other => format!("signal {other}"),
    }
}

/// Force a spawned internal process to exit and wait until it can no longer take ownership.
pub async fn terminate_internal_process(child: &InternalProcess) {
    if child.has_exited() {
        return;
    }
    kill_pid(child.pid);
    child.exited().await;
}

/// Send SIGKILL (or terminate on Windows). A missing process is not an error.
pub fn kill_pid(pid: u32) {
    #[cfg(unix)]
    if let Some(pid) = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw) {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Whether a process with this ID exists.
#[cfg(unix)]
pub fn process_exists(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw) else { return false };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        Err(error) => error == rustix::io::Errno::PERM,
    }
}

/// Encode one newline-terminated JSON control message.
pub fn encode_control_line(message: &serde_json::Value) -> ChordResult<String> {
    let line = format!("{message}\n");
    if line.len() > MAX_CONTROL_LINE_BYTES {
        return Err(ChordError::error("Internal control message is too large"));
    }
    Ok(line)
}

/// Run this process as an internal process when its environment names a role. Returns false otherwise.
pub async fn run_internal_process_from_env(args: &[String]) -> ChordResult<bool> {
    let Some(role) = consume_internal_process_role()? else { return Ok(false) };
    run_internal_process(role, args).await?;
    Ok(true)
}

#[cfg(unix)]
async fn run_internal_process(role: InternalProcessRole, args: &[String]) -> ChordResult<()> {
    match role {
        InternalProcessRole::Coordinator => super::coordinator_process::run_coordinator_process(args).await,
        InternalProcessRole::Server => super::server_runtime::run_server_process(args).await,
        InternalProcessRole::SessionWorker => super::session_worker_run::run_session_worker_process(args).await,
    }
}

#[cfg(not(unix))]
async fn run_internal_process(role: InternalProcessRole, _args: &[String]) -> ChordResult<()> {
    Err(ChordError::error(format!("The experimental {} process requires Unix sockets", role.as_str())))
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
