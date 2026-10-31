//! Passing TCP sockets and servers over the IPC channel, for `subprocess.send(message, handle)`
//! and `cluster`. Unix sends the descriptor with SCM_RIGHTS on the IPC socket; Windows
//! duplicates the socket for the peer process with `WSADuplicateSocketW`.
#![allow(unsafe_code)]

use std::sync::Mutex;

use rquickjs::{Ctx, Function, Object, Value};
use serde_json::json;

use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::{from_json, throw};
use crate::runtime::state::state;

/// A socket taken out of a `net` handle (or received from the peer) and not yet adopted.
pub(super) struct SocketHandle(pub(super) socket2::Socket);

/// Descriptors received on an IPC channel, waiting for the data event that carries them.
#[derive(Default)]
pub(super) struct Received(Mutex<Vec<socket2::Socket>>);

impl Received {
    pub(super) fn take(&self) -> Vec<socket2::Socket> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|poison| poison.into_inner()))
    }
}

pub(super) fn store(sockets: Vec<socket2::Socket>) -> Vec<u64> {
    let state = state();
    sockets.into_iter().map(|socket| state.insert_resource(Box::new(SocketHandle(socket)))).collect()
}

pub(super) fn take(ctx: &Ctx<'_>, id: u64) -> rquickjs::Result<socket2::Socket> {
    let resource = state().resources.borrow_mut().remove(&id);
    match resource.map(|resource| resource.downcast::<SocketHandle>()) {
        Some(Ok(handle)) => Ok(handle.0),
        _ => Err(throw(ctx, &JsError::new(format!("handle {id} is closed")).with_code("ERR_INVALID_HANDLE_TYPE"))),
    }
}

#[cfg(unix)]
pub(super) mod unix {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, ready};

    use tokio::io::{AsyncRead, AsyncWrite, Interest, ReadBuf};
    use tokio::net::UnixStream;

    use super::Received;

    #[cfg(target_os = "linux")]
    const RECV_FLAGS: i32 = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    const RECV_FLAGS: i32 = 0;
    #[cfg(target_os = "linux")]
    const SEND_FLAGS: i32 = libc::MSG_NOSIGNAL;
    #[cfg(not(target_os = "linux"))]
    const SEND_FLAGS: i32 = 0;

    fn recv_with_fds(fd: RawFd, buffer: &mut [u8], out: &mut Vec<OwnedFd>) -> io::Result<usize> {
        let mut iov = libc::iovec { iov_base: buffer.as_mut_ptr().cast(), iov_len: buffer.len() };
        let mut space = [0u64; 32];
        // SAFETY: msghdr points at live buffers for the call; QuickJS is not involved. Each
        // SCM_RIGHTS descriptor is new to this process and owned once.
        unsafe {
            let mut message: libc::msghdr = std::mem::zeroed();
            message.msg_iov = &mut iov;
            message.msg_iovlen = 1;
            message.msg_control = space.as_mut_ptr().cast();
            message.msg_controllen = std::mem::size_of_val(&space) as _;
            let count = libc::recvmsg(fd, &mut message, RECV_FLAGS);
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut header = libc::CMSG_FIRSTHDR(&message);
            while !header.is_null() {
                if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(header).cast::<RawFd>();
                    let length = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    for index in 0..length / std::mem::size_of::<RawFd>() {
                        out.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(index))));
                    }
                }
                header = libc::CMSG_NXTHDR(&message, header);
            }
            Ok(count as usize)
        }
    }

    fn send_with_fd(fd: RawFd, bytes: &[u8], passed: RawFd) -> io::Result<usize> {
        let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut _, iov_len: bytes.len() };
        let mut space = [0u64; 8];
        // SAFETY: as in `recv_with_fds`; the control buffer holds one aligned SCM_RIGHTS header.
        unsafe {
            let mut message: libc::msghdr = std::mem::zeroed();
            message.msg_iov = &mut iov;
            message.msg_iovlen = 1;
            message.msg_control = space.as_mut_ptr().cast();
            message.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as _;
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), passed);
            let count = libc::sendmsg(fd, &message, SEND_FLAGS);
            if count < 0 { Err(io::Error::last_os_error()) } else { Ok(count as usize) }
        }
    }

    /// Received descriptors as sockets that spawned children do not inherit. Linux receives them close-on-exec
    /// (`MSG_CMSG_CLOEXEC`); elsewhere the flag is set here, so a fork between `recvmsg` and this call still inherits them.
    fn close_on_exec(fds: Vec<OwnedFd>) -> io::Result<Vec<socket2::Socket>> {
        let sockets: Vec<socket2::Socket> = fds.into_iter().map(socket2::Socket::from).collect();
        #[cfg(not(target_os = "linux"))]
        for socket in &sockets {
            socket.set_cloexec(true)?;
        }
        Ok(sockets)
    }

    /// Reads the IPC socket, keeping descriptors that arrive with the bytes.
    pub(in crate::node) struct FdReader(pub Arc<UnixStream>, pub Arc<Received>);

    impl AsyncRead for FdReader {
        fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            loop {
                ready!(self.0.poll_read_ready(cx))?;
                let mut fds = Vec::new();
                let fd = self.0.as_raw_fd();
                let result = {
                    let unfilled = buf.initialize_unfilled();
                    self.0.try_io(Interest::READABLE, || recv_with_fds(fd, unfilled, &mut fds))
                };
                match result {
                    Ok(count) => {
                        // A failure closes the whole batch before any of it or its bytes is published.
                        let sockets = match close_on_exec(fds) {
                            Ok(sockets) => sockets,
                            Err(error) => return Poll::Ready(Err(error)),
                        };
                        buf.advance(count);
                        let mut received = self.1.0.lock().unwrap_or_else(|poison| poison.into_inner());
                        received.extend(sockets);
                        return Poll::Ready(Ok(()));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(error) => return Poll::Ready(Err(error)),
                }
            }
        }
    }

    /// Writes the IPC socket; shares the stream with `send_handle`.
    pub(in crate::node) struct FdWriter(pub Arc<UnixStream>);

    impl AsyncWrite for FdWriter {
        fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
            loop {
                ready!(self.0.poll_write_ready(cx))?;
                match self.0.try_write(data) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    result => return Poll::Ready(result),
                }
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(socket2::SockRef::from(&*self.0).shutdown(std::net::Shutdown::Write))
        }
    }

    /// Write `bytes` with `socket`'s descriptor attached to the first chunk.
    pub(in crate::node) async fn send_handle(
        stream: Arc<UnixStream>,
        bytes: Vec<u8>,
        socket: socket2::Socket,
    ) -> io::Result<()> {
        let (fd, passed) = (stream.as_raw_fd(), socket.as_raw_fd());
        let mut sent = loop {
            stream.writable().await?;
            match stream.try_io(Interest::WRITABLE, || send_with_fd(fd, &bytes, passed)) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                result => break result?,
            }
        };
        while sent < bytes.len() {
            stream.writable().await?;
            match stream.try_write(&bytes[sent..]) {
                Ok(count) => sent += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(error),
            }
        }
        drop(socket);
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::io::{AsRawSocket, FromRawSocket};

    use windows_sys::Win32::Networking::WinSock::{
        FROM_PROTOCOL_INFO, INVALID_SOCKET, SOCKET_ERROR, WSA_FLAG_OVERLAPPED, WSADATA, WSADuplicateSocketW,
        WSAGetLastError, WSAPROTOCOL_INFOW, WSASocketW, WSAStartup,
    };

    const INFO_BYTES: usize = std::mem::size_of::<WSAPROTOCOL_INFOW>();

    /// The protocol info bytes that let process `pid` open its own copy of `socket`.
    pub(super) fn duplicate(socket: &socket2::Socket, pid: u32) -> io::Result<Vec<u8>> {
        // SAFETY: `info` is a plain struct WinSock fills; it is copied out as bytes.
        unsafe {
            let mut info: WSAPROTOCOL_INFOW = std::mem::zeroed();
            if WSADuplicateSocketW(socket.as_raw_socket() as _, pid, &mut info) == SOCKET_ERROR {
                return Err(io::Error::from_raw_os_error(WSAGetLastError()));
            }
            Ok(std::slice::from_raw_parts((&info as *const WSAPROTOCOL_INFOW).cast::<u8>(), INFO_BYTES).to_vec())
        }
    }

    pub(super) fn import(bytes: &[u8]) -> io::Result<socket2::Socket> {
        if bytes.len() != INFO_BYTES {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // SAFETY: the bytes came from WSADuplicateSocketW in the peer; WSAStartup may run twice.
        unsafe {
            let mut data: WSADATA = std::mem::zeroed();
            WSAStartup(0x202, &mut data);
            let mut info: WSAPROTOCOL_INFOW = std::mem::zeroed();
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (&mut info as *mut WSAPROTOCOL_INFOW).cast::<u8>(),
                INFO_BYTES,
            );
            let socket =
                WSASocketW(FROM_PROTOCOL_INFO, FROM_PROTOCOL_INFO, FROM_PROTOCOL_INFO, &info, 0, WSA_FLAG_OVERLAPPED);
            if socket == INVALID_SOCKET {
                return Err(io::Error::from_raw_os_error(WSAGetLastError()));
            }
            Ok(socket2::Socket::from_raw_socket(socket as _))
        }
    }
}

fn export(ctx: Ctx<'_>, id: u64) -> rquickjs::Result<u64> {
    let socket = super::net::socket_of(id).ok_or_else(|| {
        throw(&ctx, &JsError::type_error("This handle type cannot be sent").with_code("ERR_INVALID_HANDLE_TYPE"))
    })?;
    let socket = socket.map_err(|error| throw(&ctx, &JsError::from_io(&error, "dup", None)))?;
    Ok(state().insert_resource(Box::new(SocketHandle(socket))))
}

fn adopt_socket<'js>(ctx: Ctx<'js>, handle: u64) -> rquickjs::Result<Value<'js>> {
    let socket = take(&ctx, handle)?;
    let _guard = state().shared.tokio.enter();
    let conn = socket
        .set_nonblocking(true)
        .and_then(|()| tokio::net::TcpStream::from_std(socket.into()))
        .and_then(super::net::tcp_conn)
        .map_err(|error| throw(&ctx, &JsError::from_io(&error, "open", None)))?;
    let (id, info) = super::net::adopt(&ctx, conn);
    from_json(&ctx, &json!({ "id": id, "info": info }))
}

fn adopt_server<'js>(ctx: Ctx<'js>, handle: u64, callback: Function<'js>) -> rquickjs::Result<Value<'js>> {
    let socket = take(&ctx, handle)?;
    super::net_server::adopt(ctx, socket, callback)
}

#[cfg(windows)]
fn duplicate(ctx: Ctx<'_>, handle: u64, pid: u32) -> rquickjs::Result<String> {
    use base64::Engine;
    let socket = take(&ctx, handle)?;
    let bytes =
        windows::duplicate(&socket, pid).map_err(|error| throw(&ctx, &JsError::from_io(&error, "dup", None)))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

#[cfg(windows)]
fn import(ctx: Ctx<'_>, text: String) -> rquickjs::Result<u64> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(text).unwrap_or_default();
    let socket = windows::import(&bytes).map_err(|error| throw(&ctx, &JsError::from_io(&error, "open", None)))?;
    Ok(store(vec![socket])[0])
}

pub(super) fn natives<'js>(object: &Object<'js>) -> rquickjs::Result<()> {
    set_fn(object, "exportHandle", export)?;
    set_fn(object, "closeHandle", |id: u64| drop(state().resources.borrow_mut().remove(&id)))?;
    set_fn(object, "adoptSocket", adopt_socket)?;
    set_fn(object, "adoptServer", adopt_server)?;
    #[cfg(windows)]
    {
        set_fn(object, "duplicateHandle", duplicate)?;
        set_fn(object, "importHandle", import)?;
    }
    Ok(())
}
