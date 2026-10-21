//! Reaching a machine with the system `ssh` (upstream `ssh.ts`): arguments, host keys, detection, deployment, and
//! connections that start the daemon over `ssh`.

mod deploy;
mod host_keys;

use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::connection::{Connection, ConnectionOptions, LogFn, lock};

pub use deploy::{
    RemoteArch, RemotePlatform, RemoteSystem, WindowsShell, default_daemon_dir, deploy_daemon, detect_platform,
    local_daemon, packaged_daemon,
};
pub use host_keys::{ScannedHostKey, accept_host_key, forget_host_key, scan_host_key};

/// How to reach a machine with the system `ssh`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshTarget {
    /// Host name or `~/.ssh/config` alias.
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
    /// Host keys this application trusts; `accept_host_key` adds to it.
    pub known_hosts_file: String,
    /// The name keys are stored under, independent of aliases, ports and jump hosts, e.g. `pi-env-<env name>`.
    pub host_key_alias: String,
    /// The `ssh` program; default `ssh`.
    pub ssh: Option<String>,
    /// An `ssh` configuration file instead of `~/.ssh/config` (`-F`).
    pub config_file: Option<String>,
}

impl SshTarget {
    pub fn new(
        host: impl Into<String>,
        known_hosts_file: impl Into<String>,
        host_key_alias: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            known_hosts_file: known_hosts_file.into(),
            host_key_alias: host_key_alias.into(),
            ..Self::default()
        }
    }

    fn program(&self) -> &str {
        self.ssh.as_deref().unwrap_or("ssh")
    }
}

/// Why an `ssh` step failed. Upstream's `HostKeyUnknownError`, `HostKeyChangedError` and `SshError` are variants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshError {
    /// The host's key is not in `known_hosts_file`; `scan_host_key` shows it so the owner can accept it.
    HostKeyUnknown(String),
    /// The host's key differs from the trusted one; it is never accepted automatically.
    HostKeyChanged(String),
    /// An `ssh` invocation that failed for another reason, with its exit code and diagnostics.
    Ssh { message: String, exit_code: Option<i32>, stderr: String },
    /// Invalid input, an unsupported system, or a local file error.
    Other(String),
}

impl fmt::Display for SshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostKeyUnknown(message) | Self::HostKeyChanged(message) | Self::Other(message) => {
                f.write_str(message)
            }
            Self::Ssh { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for SshError {}

/// No leading `-` (it would be read as an `ssh` option) and no whitespace or control characters.
fn check_field(name: &str, value: &str) -> Result<(), SshError> {
    if value.is_empty() || value.starts_with('-') || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(SshError::Other(format!("Invalid {name}: {}", serde_json::json!(value))));
    }
    Ok(())
}

/// A path for an `ssh` option that expands `%` tokens: quoted, with `%` literal.
fn config_path(name: &str, path: &str) -> Result<String, SshError> {
    if path.is_empty() || path.chars().any(|c| c == '"' || c.is_control()) {
        return Err(SshError::Other(format!("Invalid {name}: {}", serde_json::json!(path))));
    }
    Ok(format!("\"{}\"", path.replace('%', "%%")))
}

/// Arguments for `ssh` up to the host: no prompts, no forwarding of any kind, no shared connections, no commands from
/// the configuration, no locale forwarding, and host keys checked strictly against the application's own file under
/// a fixed alias. `known_hosts_file` overrides the target's file.
pub fn ssh_arguments(
    target: &SshTarget,
    strict_host_keys: bool,
    known_hosts_file: Option<&str>,
) -> Result<Vec<String>, SshError> {
    check_field("host", &target.host)?;
    check_field("host key alias", &target.host_key_alias)?;
    if let Some(user) = &target.user {
        check_field("user", user)?;
    }
    if target.port == Some(0) {
        return Err(SshError::Other("Invalid port: 0".into()));
    }
    let known_hosts = config_path("known hosts file", known_hosts_file.unwrap_or(&target.known_hosts_file))?;
    let mut args: Vec<String> = Vec::new();
    if let Some(config) = &target.config_file {
        args.extend(["-F".into(), config.clone()]);
    }
    args.extend(["-T", "-a", "-x"].map(String::from));
    let checking = if strict_host_keys { "yes" } else { "accept-new" };
    let options = [
        "BatchMode=yes".to_string(),
        "ClearAllForwardings=yes".into(),
        "ForwardAgent=no".into(),
        "ForwardX11=no".into(),
        "ControlMaster=no".into(),
        "ControlPath=none".into(),
        "RemoteCommand=none".into(),
        "PermitLocalCommand=no".into(),
        "SendEnv=-*".into(),
        "ServerAliveInterval=15".into(),
        format!("StrictHostKeyChecking={checking}"),
        format!("UserKnownHostsFile={known_hosts}"),
        "GlobalKnownHostsFile=none".into(),
        "HashKnownHosts=no".into(),
        format!("HostKeyAlias={}", target.host_key_alias),
    ];
    for option in options {
        args.extend(["-o".into(), option]);
    }
    if let Some(user) = &target.user {
        args.extend(["-l".into(), user.clone()]);
    }
    if let Some(port) = target.port {
        args.extend(["-p".into(), port.to_string()]);
    }
    if let Some(identity) = &target.identity_file {
        let identity = config_path("identity file", identity)?;
        args.extend(["-o".into(), format!("IdentityFile={identity}"), "-o".into(), "IdentitiesOnly=yes".into()]);
    }
    args.extend(["--".into(), target.host.clone()]);
    Ok(args)
}

/// How long a detection, check or upload over `ssh` may take before it is abandoned.
const SSH_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

fn ssh_failure(message: String) -> SshError {
    SshError::Ssh { message, exit_code: None, stderr: String::new() }
}

/// Run one remote command over `ssh`, optionally feeding stdin; stdout on success.
pub(crate) async fn run_ssh(
    target: &SshTarget,
    command: &str,
    stdin: Option<Vec<u8>>,
    args: Option<Vec<String>>,
    timeout: Option<Duration>,
) -> Result<String, SshError> {
    let args = match args {
        Some(args) => args,
        None => ssh_arguments(target, true, None)?,
    };
    let mut child = Command::new(target.program())
        .args(&args)
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ssh_failure(error.to_string()))?;
    if let Some(mut input) = child.stdin.take() {
        let bytes = stdin.unwrap_or_default();
        tokio::spawn(async move {
            let _ = input.write_all(&bytes).await;
            let _ = input.shutdown().await;
        });
    }
    let limit = timeout.unwrap_or(SSH_TIMEOUT);
    let Ok(output) = tokio::time::timeout(limit, child.wait_with_output()).await else {
        return Err(ssh_failure(format!("ssh {} did not finish within {} s", target.host, limit.as_secs())));
    };
    let output = output.map_err(|error| ssh_failure(error.to_string()))?;
    let diagnostics = String::from_utf8_lossy(&output.stderr).into_owned();
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let host = &target.host;
    if diagnostics.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return Err(SshError::HostKeyChanged(format!(
            "The host key of {host} changed; remove the old key to continue"
        )));
    }
    let unknown_key = diagnostics
        .lines()
        .any(|line| line.find("No ").is_some_and(|index| line[index + 3..].contains(" host key is known")));
    if diagnostics.contains("Host key verification failed") || unknown_key {
        return Err(SshError::HostKeyUnknown(format!("The host key of {host} is not trusted yet")));
    }
    let code = output.status.code();
    let code_text = code.map_or("null".into(), |code| code.to_string());
    let message = format!("ssh {host} failed with exit code {code_text}: {}", diagnostics.trim());
    Err(SshError::Ssh { message, exit_code: code, stderr: diagnostics })
}

/// Options of `ssh_connection` and `connect_ssh`.
#[derive(Clone, Default)]
pub struct SshConnectOptions {
    pub target: SshTarget,
    /// The binary to deploy; default: `packaged_daemon` in `daemon_dir`.
    pub binary: Option<PathBuf>,
    /// Where the daemons for each remote system are; default `default_daemon_dir()`.
    pub daemon_dir: Option<PathBuf>,
    /// POSIX: start the daemon through the user's login shell (`$SHELL -l`). Off by default.
    pub login_shell: bool,
    pub on_log: Option<LogFn>,
}

impl SshConnectOptions {
    pub fn new(target: SshTarget) -> Self {
        Self { target, ..Self::default() }
    }

    fn binary_for(&self, remote: &RemotePlatform) -> PathBuf {
        self.binary.clone().unwrap_or_else(|| packaged_daemon(remote, self.daemon_dir.as_deref()))
    }

    fn log_warnings(&self, remote: &RemotePlatform) {
        if let Some(on_log) = &self.on_log {
            for warning in &remote.warnings {
                on_log(&format!("{warning}\n"));
            }
        }
    }
}

/// What an SSH connection learned about its remote system, and the daemon file its last deployment checked.
#[derive(Default)]
struct SshState {
    remote: Option<RemotePlatform>,
    checked: Option<String>,
}

/// A `Connection` whose every start detects the remote system (once), checks or deploys the daemon, then starts it.
fn ssh_connection_for(options: SshConnectOptions, state: Arc<Mutex<SshState>>) -> Connection {
    let on_log = options.on_log.clone();
    let options = Arc::new(options);
    let command = Arc::new(move || {
        let (options, state) = (options.clone(), state.clone());
        async move {
            let known = lock(&state).remote.clone();
            let remote = match known {
                Some(remote) => remote,
                None => {
                    let remote = detect_platform(&options.target).await.map_err(|error| error.to_string())?;
                    options.log_warnings(&remote);
                    lock(&state).remote = Some(remote.clone());
                    remote
                }
            };
            // The file a deployment just checked serves the first start.
            let checked = lock(&state).checked.take();
            let file = match checked {
                Some(file) => file,
                None => deploy_daemon(&options.target, &remote, &options.binary_for(&remote))
                    .await
                    .map_err(|error| error.to_string())?,
            };
            let mut command = vec![options.target.program().to_string()];
            command.extend(ssh_arguments(&options.target, true, None).map_err(|error| error.to_string())?);
            command.push(deploy::launch_command(&remote, &file, options.login_shell));
            Ok(command)
        }
        .boxed()
    });
    let mut connection_options = ConnectionOptions::computed(command);
    connection_options.on_log = on_log;
    Connection::new(connection_options)
}

/// A lazy SSH connection and the remote system it detected, once known.
pub struct SshConnection {
    pub connection: Connection,
    state: Arc<Mutex<SshState>>,
}

impl SshConnection {
    pub fn remote(&self) -> Option<RemotePlatform> {
        lock(&self.state).remote.clone()
    }
}

/// A `Connection` to the target that does nothing until its first request. Each start detects the remote system (the
/// first time), checks or deploys the daemon, then starts it over `ssh`. A failure fails the waiting requests with
/// code `spawn_error` and the `ssh` diagnostics as message; the next request tries again.
pub fn ssh_connection(options: SshConnectOptions) -> SshConnection {
    let state = Arc::new(Mutex::new(SshState::default()));
    SshConnection { connection: ssh_connection_for(options, state.clone()), state }
}

/// Detect the remote system and deploy the daemon now, then return a `Connection` that starts it over `ssh`. Host
/// keys must already be trusted (`scan_host_key`, `accept_host_key`), otherwise this fails with `HostKeyUnknown`.
pub async fn connect_ssh(options: SshConnectOptions) -> Result<(Connection, RemotePlatform), SshError> {
    let remote = detect_platform(&options.target).await?;
    let file = deploy_daemon(&options.target, &remote, &options.binary_for(&remote)).await?;
    options.log_warnings(&remote);
    let state = SshState { remote: Some(remote.clone()), checked: Some(file) };
    Ok((ssh_connection_for(options, Arc::new(Mutex::new(state))), remote))
}
