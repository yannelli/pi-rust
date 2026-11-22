//! One exchange with one server: a connected UDP socket, or TCP with the RFC 1035 4.2.2
//! two-byte length prefix. Each socket lives only for its exchange.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, UdpSocket};
use tokio::time::{Instant, timeout_at};

use super::super::wire::{Question, Reply};
use super::{BADRESP, CONNREFUSED, TIMEOUT};

/// Largest UDP payload, so an oversized datagram is never cut short.
const DATAGRAM: usize = 65_535;

pub(super) struct Attempt<'a> {
    pub(super) question: &'a Question,
    pub(super) packet: Vec<u8>,
    pub(super) id: u16,
    pub(super) ttl: bool,
    pub(super) server: SocketAddr,
    pub(super) source: IpAddr,
}

fn io_error(_: std::io::Error) -> &'static str {
    CONNREFUSED
}

impl Attempt<'_> {
    /// Sends one datagram and waits until `wait` ends. Packets the codec reports as unrelated
    /// (other ID or question) are dropped and do not move the deadline.
    pub(super) async fn udp(&self, wait: Duration) -> Result<Reply, &'static str> {
        let deadline = Instant::now() + wait;
        let exchange = async {
            let socket = UdpSocket::bind(SocketAddr::new(self.source, 0)).await.map_err(io_error)?;
            socket.connect(self.server).await.map_err(io_error)?;
            socket.send(&self.packet).await.map_err(io_error)?;
            let mut buffer = BytesMut::with_capacity(DATAGRAM);
            loop {
                buffer.clear();
                socket.recv_buf(&mut buffer).await.map_err(io_error)?;
                match self.question.answer(self.id, &buffer, self.ttl)? {
                    Reply::Unrelated => continue,
                    reply => return Ok(reply),
                }
            }
        };
        timeout_at(deadline, exchange).await.unwrap_or(Err(TIMEOUT))
    }

    /// Connects, sends the framed query, and reads framed replies until `wait` ends.
    pub(super) async fn tcp(&self, wait: Duration) -> Result<Reply, &'static str> {
        let deadline = Instant::now() + wait;
        let exchange = async {
            let socket = match self.server {
                SocketAddr::V4(_) => TcpSocket::new_v4(),
                SocketAddr::V6(_) => TcpSocket::new_v6(),
            }
            .map_err(io_error)?;
            socket.bind(SocketAddr::new(self.source, 0)).map_err(io_error)?;
            let mut stream = socket.connect(self.server).await.map_err(io_error)?;
            let length = u16::try_from(self.packet.len()).map_err(|_| "EBADQUERY")?;
            let mut framed = Vec::with_capacity(self.packet.len() + 2);
            framed.extend_from_slice(&length.to_be_bytes());
            framed.extend_from_slice(&self.packet);
            stream.write_all(&framed).await.map_err(io_error)?;
            loop {
                let mut prefix = [0u8; 2];
                stream.read_exact(&mut prefix).await.map_err(io_error)?;
                let mut body = vec![0u8; usize::from(u16::from_be_bytes(prefix))];
                stream.read_exact(&mut body).await.map_err(io_error)?;
                match self.question.answer(self.id, &body, self.ttl)? {
                    Reply::Unrelated => continue,
                    Reply::Truncated => return Err(BADRESP),
                    reply => return Ok(reply),
                }
            }
        };
        timeout_at(deadline, exchange).await.unwrap_or(Err(TIMEOUT))
    }
}
