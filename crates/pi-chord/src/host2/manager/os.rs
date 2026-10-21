//! What differs per OS: how the parent and child reach one channel, process groups and exit statuses.
//! Unix: the channel is a socket pair, given to the child as its stdin (Node's IPC uses a socket pair too).
//! Windows: a named pipe the child opens by name. Neither needs code that could not be safe Rust.

use std::io;
use std::process::ExitStatus;
use std::sync::Arc;

use tokio::process::Command;
use tokio::sync::watch;

use super::Exit;
use super::control::IPC_ENV;
use crate::host2::link::StreamChannel;

#[cfg(unix)]
pub(super) fn parent_channel(
    command: &mut Command,
    _exit: watch::Receiver<Option<Exit>>,
) -> io::Result<Arc<StreamChannel>> {
    use std::os::fd::OwnedFd;
    use std::process::Stdio;

    let (parent, child) = std::os::unix::net::UnixStream::pair()?;
    parent.set_nonblocking(true)?;
    let parent = tokio::net::UnixStream::from_std(parent)?;
    command.stdin(Stdio::from(OwnedFd::from(child)));
    command.env(IPC_ENV, "stdin");
    Ok(StreamChannel::new(async move { Ok(parent) }))
}

/// Takes the channel from stdin, which becomes /dev/null: what this process spawns must not inherit the channel.
#[cfg(unix)]
pub(super) fn child_channel(address: &str) -> io::Result<Arc<StreamChannel>> {
    use std::os::fd::AsFd;

    if address != "stdin" {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{IPC_ENV}={address}: expected stdin")));
    }
    let stream = std::os::unix::net::UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    stream.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(stream)?;
    rustix::stdio::dup2_stdin(std::fs::File::open("/dev/null")?)?;
    Ok(StreamChannel::new(async move { Ok(stream) }))
}

/// The child leads a new process group, so the whole tree can be killed.
#[cfg(unix)]
pub(super) fn detach(command: &mut Command) {
    command.process_group(0);
}

/// Kills the process group `pid` leads; `false` when there is none.
#[cfg(unix)]
pub(super) fn kill_group(pid: u32) -> bool {
    use rustix::process::{Pid, Signal, kill_process_group};

    let pid = i32::try_from(pid).ok().and_then(Pid::from_raw);
    pid.is_some_and(|pid| kill_process_group(pid, Signal::KILL).is_ok())
}

#[cfg(unix)]
pub(super) fn exit_of(status: ExitStatus) -> Exit {
    use std::os::unix::process::ExitStatusExt;

    Exit { code: status.code(), signal: status.signal().map(signal_name) }
}

#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    let name = match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        6 => "SIGABRT",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        other => return format!("SIG{other}"),
    };
    name.into()
}

#[cfg(windows)]
pub(super) fn parent_channel(
    command: &mut Command,
    exit: watch::Receiver<Option<Exit>>,
) -> io::Result<Arc<StreamChannel>> {
    use std::process::Stdio;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::net::windows::named_pipe::ServerOptions;

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.subsec_nanos());
    let next = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = format!(r"\\.\pipe\chord2-ipc-{}-{next}-{nanos}", std::process::id());
    let server = ServerOptions::new().first_pipe_instance(true).create(&name)?;
    command.env(IPC_ENV, &name);
    command.stdin(Stdio::null());
    Ok(StreamChannel::new(async move {
        // A child that exits before it connects never will.
        let connected = tokio::select! {
            connected = server.connect() => connected,
            _ = super::fork::exit_value(exit) => Err(io::Error::other("child exited")),
        };
        connected.map(|()| server)
    }))
}

#[cfg(windows)]
pub(super) fn child_channel(address: &str) -> io::Result<Arc<StreamChannel>> {
    let client = tokio::net::windows::named_pipe::ClientOptions::new().open(address)?;
    Ok(StreamChannel::new(async move { Ok(client) }))
}

/// Node's `detached` on Windows: a new process group (Windows has no process trees to kill).
#[cfg(windows)]
pub(super) fn detach(command: &mut Command) {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(windows)]
pub(super) fn kill_group(_pid: u32) -> bool {
    false
}

#[cfg(windows)]
pub(super) fn exit_of(status: ExitStatus) -> Exit {
    Exit { code: status.code(), signal: None }
}
