//! One exchange with one server: a connected UDP socket that lives only for the exchange, or a
//! question on the server's shared TCP stream.

use std::cell::RefCell;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, UdpSocket as StdUdpSocket};

use bytes::BytesMut;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::Interest;
use tokio::net::UdpSocket;

use super::super::wire::{Question, Reply};
use super::CONNREFUSED;
use super::tcp::TcpPool;
use super::timer::Running;

thread_local! {
    // Only the synchronous receive and parse borrow this buffer. Pending queries share it on each worker.
    static DATAGRAM: RefCell<BytesMut> = RefCell::new(BytesMut::with_capacity(usize::from(u16::MAX)));
}

pub(super) struct Attempt<'a> {
    pub(super) question: &'a Question,
    pub(super) packet: &'a [u8],
    pub(super) id: u16,
    pub(super) ttl: bool,
    pub(super) server: SocketAddr,
    pub(super) source: IpAddr,
}

fn io_error(_: std::io::Error) -> &'static str {
    CONNREFUSED
}

impl Attempt<'_> {
    /// Sends a connected datagram and ignores replies to other questions.
    pub(super) async fn udp(&self, timer: &Running<'_>) -> Result<Reply, &'static str> {
        let raw = Socket::new(Domain::for_address(self.server), Type::DGRAM, Some(Protocol::UDP)).map_err(io_error)?;
        if !self.source.is_unspecified() {
            raw.bind(&SocketAddr::new(self.source, 0).into()).map_err(io_error)?;
        }
        let socket = StdUdpSocket::from(raw);
        socket.set_nonblocking(true).map_err(io_error)?;
        socket.connect(self.server).map_err(io_error)?;
        let sent = match socket.send(self.packet) {
            Ok(_) => true,
            Err(error) if error.kind() == ErrorKind::WouldBlock => false,
            Err(error) => return Err(io_error(error)),
        };
        let socket = UdpSocket::from_std(socket).map_err(io_error)?;
        if !sent {
            socket.send(self.packet).await.map_err(io_error)?;
        }
        loop {
            let ready = socket.ready(Interest::READABLE | Interest::ERROR).await.map_err(io_error)?;
            if ready.is_error() {
                // Linux can report a connected UDP port refusal as EPOLLERR without EPOLLIN.
                match socket.try_io(Interest::ERROR, || socket.take_error()?.ok_or(ErrorKind::WouldBlock.into())) {
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                    Ok(error) | Err(error) => return Err(io_error(error)),
                }
            }
            let received = DATAGRAM.with_borrow_mut(|buffer| {
                buffer.clear();
                socket.try_recv_buf(buffer).map(|_| self.question.answer(self.id, buffer, self.ttl))
            });
            match received {
                Ok(Ok(Reply::Unrelated)) => timer.activity(),
                Ok(reply) => return reply,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => return Err(io_error(error)),
            }
        }
    }

    pub(super) async fn tcp(&self, pool: &TcpPool) -> Result<Reply, &'static str> {
        pool.send(self)?.reply().await
    }
}
