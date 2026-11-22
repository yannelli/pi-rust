//! The task behind one shared stream: connect, write queued questions, and settle each
//! question with the first reply frame its codec accepts.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use bytes::{Buf, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::super::super::wire::Reply;
use super::super::BADRESP;
use super::{Key, Link, Queued, State, lock};

/// c-ares cannot parse a message shorter than the DNS header and fails the connection.
const HEADER: usize = 12;
/// Read buffer for the first replies; a larger frame grows it to its RFC 1035 length.
const READ_BUFFER: usize = 2048;

type Queue = mpsc::UnboundedReceiver<Queued>;

/// Removes the link when its task ends for any reason, which settles every waiter left with
/// `ECONNREFUSED`.
struct Release {
    shared: Arc<Mutex<State>>,
    key: Key,
    serial: u64,
}

impl Drop for Release {
    fn drop(&mut self) {
        let removed = {
            let mut state = lock(&self.shared);
            state.remove(self.key, self.serial)
        };
        drop(removed);
    }
}

pub(super) async fn run(shared: Arc<Mutex<State>>, key: Key, serial: u64, mut frames: Queue, close: CancellationToken) {
    let release = Release { shared, key, serial };
    tokio::select! {
        biased;
        () = close.cancelled() => {}
        _ = drive(&release, &mut frames) => {}
    }
}

async fn connect(source: IpAddr, server: SocketAddr) -> io::Result<TcpStream> {
    let socket = if server.is_ipv4() { TcpSocket::new_v4() } else { TcpSocket::new_v6() }?;
    if !source.is_unspecified() {
        socket.bind(SocketAddr::new(source, 0))?;
    }
    let stream = socket.connect(server).await?;
    // Questions that join the stream are small writes that must not wait for an ACK.
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// Moves the queued frames of questions that still wait into `batch`; the first is moved as is.
fn collect(release: &Release, frames: &mut Queue, first: Queued, batch: &mut Vec<u8>) {
    let mut state = lock(&release.shared);
    let Some(link) = state.link(release.key, release.serial) else { return };
    let mut queued = Some(first);
    while let Some(Queued { wire, token, frame }) = queued.take().or_else(|| frames.try_recv().ok()) {
        if !link.owns(wire, token) {
            continue;
        }
        if batch.is_empty() { *batch = frame } else { batch.extend_from_slice(&frame) }
    }
}

/// Runs until the peer closes the stream, an I/O error, or a malformed frame; `run` also stops
/// it when the last question leaves.
async fn drive(release: &Release, frames: &mut Queue) -> io::Result<()> {
    let (source, server) = release.key;
    let mut stream = connect(source, server).await?;
    let (mut reader, mut writer) = stream.split();
    let mut input = BytesMut::with_capacity(READ_BUFFER);
    let mut batch = Vec::new();
    let mut sent = 0;
    loop {
        tokio::select! {
            queued = frames.recv(), if batch.is_empty() => {
                let Some(first) = queued else { return Ok(()) };
                collect(release, frames, first, &mut batch);
            }
            written = writer.write(&batch[sent..]), if !batch.is_empty() => {
                match written? {
                    0 => return Err(io::ErrorKind::WriteZero.into()),
                    count => sent += count,
                }
                if sent == batch.len() {
                    batch.clear();
                    sent = 0;
                }
            }
            read = reader.read_buf(&mut input) => {
                if read? == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                if !dispatch(release, &mut input) {
                    return Ok(());
                }
            }
        }
    }
}

/// Settles questions with the complete frames in `input` and keeps a partial frame for the next
/// read. Returns `false` after a malformed frame, which fails every question on the stream.
fn dispatch(release: &Release, input: &mut BytesMut) -> bool {
    let mut state = lock(&release.shared);
    let Some(link) = state.link(release.key, release.serial) else { return false };
    while input.len() >= 2 {
        let end = 2 + usize::from(u16::from_be_bytes([input[0], input[1]]));
        if input.len() < end {
            input.reserve(end - input.len());
            break;
        }
        let mut frame = input.split_to(end);
        frame.advance(2);
        if frame.is_empty() {
            continue;
        }
        if frame.len() < HEADER {
            fail(link, BADRESP);
            return false;
        }
        let wire = u16::from_be_bytes([frame[0], frame[1]]);
        let Some(waiter) = link.waiters.get(&wire) else {
            // c-ares parses a reply before it looks up the ID; any question's codec can do that
            // when asked with an ID the frame does not carry.
            let malformed = link
                .waiters
                .values()
                .next()
                .and_then(|other| other.question.answer_tcp(wire.wrapping_add(1), &frame, false).err());
            if let Some(code) = malformed {
                fail(link, code);
                return false;
            }
            continue;
        };
        frame[..2].copy_from_slice(&waiter.id.to_be_bytes());
        let outcome = match waiter.question.answer_tcp(waiter.id, &frame, waiter.ttl) {
            Ok(Reply::Unrelated) => continue,
            Ok(Reply::Truncated) => Err(BADRESP),
            Ok(reply) => Ok(reply),
            Err(code) => {
                fail(link, code);
                return false;
            }
        };
        if let Some(waiter) = link.waiters.remove(&wire) {
            let _ = waiter.done.send(outcome);
        }
    }
    let idle = state.close_if_idle(release.key, release.serial);
    drop(state);
    drop(idle);
    true
}

/// Settles every question on the stream with `code`, as c-ares does for a connection error.
fn fail(link: &mut Link, code: &'static str) {
    for (_, waiter) in link.waiters.drain() {
        let _ = waiter.done.send(Err(code));
    }
}
