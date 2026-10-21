//! Remote execution environments for Pi Durable: `RemoteExecutionEnv` runs a Durable `ExecutionEnv` on another host
//! through the `pi-env` daemon (crate `pi-env-daemon`). Upstream ships daemons in its npm `bin/`. Here deployment
//! reads `<daemon_dir>/pi-env-<platform>-<arch>/pi-env[.exe]`; `daemon_dir` defaults to `pi-env-daemons` beside the exe.

mod connection;
mod errors;
mod frame;
mod list_files;
mod paths;
mod remote_env;
mod session;
mod ssh;
mod terminal;
mod watch;

pub use connection::{
    CommandFn, Connection, ConnectionOptions, DaemonCommand, EventFn, Json, LogFn, NodePlatform, RemoteError,
    RemoteInfo, Reply, RequestOptions, is_connection_lost, node_platform,
};
pub use errors::to_file_error;
pub use list_files::{LocalExecutionEnv, list_files_over};
pub use paths::PathStyle;
pub use pi_durable::env::{FileListEntry, FileListing, ListFilesOptions};
pub use remote_env::{RemoteExecutionEnv, RemoteExecutionEnvOptions, remote_path};
pub use ssh::{
    RemoteArch, RemotePlatform, RemoteSystem, ScannedHostKey, SshConnectOptions, SshConnection, SshError, SshTarget,
    WindowsShell, accept_host_key, connect_ssh, default_daemon_dir, deploy_daemon, detect_platform, forget_host_key,
    local_daemon, packaged_daemon, scan_host_key, ssh_arguments, ssh_connection,
};
pub use terminal::{DataFn, RemoteTerminal, TerminalForeground, TerminalOptions, open_terminal};
pub use watch::{RemoteWatchOptions, RemoteWatcher};
