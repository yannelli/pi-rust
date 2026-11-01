//! Child stdio entries beyond `pipe`, `ignore`, and `inherit`: open files and TCP sockets at any
//! index, and extra pipes above 2. On Unix the child moves each descriptor into place before exec.
#![allow(unsafe_code)]

use std::process::{Command, Stdio};

use rquickjs::Ctx;
use serde::Deserialize;

use crate::error::JsError;
#[cfg(unix)]
use crate::runtime::convert::throw;
use crate::runtime::state::state;

/// One stdio entry: a kind, a descriptor from `fs.open`, or a `net` socket handle.
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum Entry {
    Kind(String),
    File {
        file: u64,
    },
    Socket {
        #[cfg_attr(windows, allow(dead_code))]
        socket: u64,
    },
}

#[cfg(unix)]
type Owned = std::os::fd::OwnedFd;
#[cfg(windows)]
type Owned = std::os::windows::io::OwnedHandle;

/// The parent's end of an extra pipe.
#[cfg(unix)]
pub(super) type ParentEnd = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub(super) type ParentEnd = std::convert::Infallible;

fn kind_stdio(kind: &str) -> Stdio {
    match kind {
        "pipe" => Stdio::piped(),
        "inherit" => Stdio::inherit(),
        _ => Stdio::null(),
    }
}

/// A copy of the file or socket an entry names, which the child receives.
fn descriptor(entry: &Entry) -> Result<Option<Owned>, JsError> {
    match entry {
        Entry::Kind(_) => Ok(None),
        Entry::File { file } => {
            let file = state().shared.files.get(*file, "spawn")?;
            let file = file.lock().unwrap_or_else(|poison| poison.into_inner()).try_clone();
            Ok(Some(file.map_err(|error| JsError::from_io(&error, "spawn", None))?.into()))
        }
        #[cfg(unix)]
        Entry::Socket { socket } => {
            let socket = super::net::socket_of(*socket, true)
                .ok_or_else(|| JsError::unsupported("child_process stdio pipe sockets"))?;
            Ok(Some(socket.map_err(|error| JsError::from_io(&error, "spawn", None))?.into()))
        }
        #[cfg(windows)]
        Entry::Socket { .. } => Err(JsError::unsupported("child_process stdio sockets on Windows")),
    }
}

/// Set up every stdio entry on `command`, returning the parent's ends of extra pipes.
pub(super) fn apply(command: &mut Command, entries: &[Entry]) -> Result<Vec<(usize, ParentEnd)>, JsError> {
    #[cfg_attr(windows, allow(unused_mut))]
    let mut pipes = Vec::new();
    #[cfg(unix)]
    let mut moves: Vec<(i32, Owned)> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let stdio = match (descriptor(entry)?, entry) {
            #[cfg(unix)]
            (Some(fd), _) => {
                moves.push((index as i32, fd));
                (index < 3).then(Stdio::inherit)
            }
            #[cfg(windows)]
            (Some(handle), _) if index < 3 => Some(Stdio::from(handle)),
            #[cfg(windows)]
            (Some(_), _) => return Err(JsError::unsupported("child_process stdio descriptors above 2 on Windows")),
            (None, Entry::Kind(kind)) if index < 3 => Some(kind_stdio(kind)),
            #[cfg(unix)]
            (None, Entry::Kind(kind)) if kind == "pipe" => {
                let (parent, child) =
                    std::os::unix::net::UnixStream::pair().map_err(|error| JsError::from_io(&error, "spawn", None))?;
                pipes.push((index, parent));
                moves.push((index as i32, child.into()));
                None
            }
            _ => None,
        };
        match (index, stdio) {
            (0, Some(stdio)) => _ = command.stdin(stdio),
            (1, Some(stdio)) => _ = command.stdout(stdio),
            (2, Some(stdio)) => _ = command.stderr(stdio),
            _ => {}
        }
    }
    #[cfg(unix)]
    if !moves.is_empty() {
        install(command, moves);
    }
    Ok(pipes)
}

/// Move each descriptor to its index in the child. Every source is first copied above the
/// highest index, so one move cannot overwrite another source; the copies close on exec.
#[cfg(unix)]
fn install(command: &mut Command, moves: Vec<(i32, Owned)>) {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let floor = moves.iter().map(|(target, _)| *target).max().unwrap_or(0).max(2) + 1;
    let mut copies = vec![0; moves.len()];
    // SAFETY: the hook runs between fork and exec and only calls fcntl and dup2, which are
    // async-signal-safe, without allocating. `moves` stays open until the Command is dropped.
    unsafe {
        command.pre_exec(move || {
            for (copy, (_, fd)) in copies.iter_mut().zip(&moves) {
                *copy = libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, floor);
                if *copy < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            for (copy, (target, _)) in copies.iter().zip(&moves) {
                if libc::dup2(*copy, *target) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Like libuv, stdin, stdout, and stderr start blocking even when shared with a socket.
                if *target <= 2 {
                    let flags = libc::fcntl(*target, libc::F_GETFL);
                    if flags >= 0 {
                        libc::fcntl(*target, libc::F_SETFL, flags & !libc::O_NONBLOCK);
                    }
                }
            }
            Ok(())
        });
    }
}

/// Register the parent's end of each extra pipe as a `net` socket: `[index, id, info]` entries.
#[cfg(unix)]
pub(super) fn adopt(ctx: &Ctx<'_>, pipes: Vec<(usize, ParentEnd)>) -> rquickjs::Result<serde_json::Value> {
    let _guard = state().shared.tokio.enter();
    let mut list = Vec::new();
    for (index, end) in pipes {
        let stream = end
            .set_nonblocking(true)
            .and_then(|()| tokio::net::UnixStream::from_std(end))
            .map_err(|error| throw(ctx, &JsError::from_io(&error, "spawn", None)))?;
        let (id, info) = super::net::adopt(ctx, super::net::pipe_conn(Box::new(stream), ""));
        list.push(serde_json::json!([index, id, info]));
    }
    Ok(serde_json::Value::Array(list))
}

#[cfg(windows)]
pub(super) fn adopt(_ctx: &Ctx<'_>, pipes: Vec<(usize, ParentEnd)>) -> rquickjs::Result<serde_json::Value> {
    Ok(pipes.into_iter().map(|(_, end)| -> serde_json::Value { match end {} }).collect())
}

type Reader = std::thread::JoinHandle<Vec<u8>>;

/// Read each extra pipe to its end on a thread, for `spawnSync`.
#[cfg(unix)]
pub(super) fn drain(pipes: Vec<(usize, ParentEnd)>) -> Vec<(usize, Reader)> {
    let read = |end: ParentEnd| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut &end, &mut bytes);
            bytes
        })
    };
    pipes.into_iter().map(|(index, end)| (index, read(end))).collect()
}

#[cfg(windows)]
pub(super) fn drain(pipes: Vec<(usize, ParentEnd)>) -> Vec<(usize, Reader)> {
    pipes.into_iter().map(|(_, end)| -> (usize, Reader) { match end {} }).collect()
}
