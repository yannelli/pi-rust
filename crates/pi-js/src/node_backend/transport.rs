//! The loopback socket that carries the protocol, and the child's raw stdout and stderr. The
//! first connection that sends the token as its first line carries the frames.

use std::io::{ErrorKind, Read};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ring::rand::{SecureRandom, SystemRandom};

use crate::error::JsError;
use crate::runtime::{OutputSink, OutputStream};

pub(crate) const PORT_ENV: &str = "PI_NODE_BACKEND_PORT";
pub(crate) const TOKEN_ENV: &str = "PI_NODE_BACKEND_TOKEN";

const TOKEN_BYTES: usize = 32;
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

fn start_error(message: impl Into<String>) -> JsError {
    JsError::new(message).with_code("ERR_PI_NODE_START")
}

fn io_error(error: std::io::Error, what: &str) -> JsError {
    start_error(format!("cannot {what} for the Node backend: {error}"))
}

pub(crate) struct Listener {
    listener: TcpListener,
    token: String,
}

impl Listener {
    pub fn bind() -> Result<Self, JsError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|error| io_error(error, "listen"))?;
        listener.set_nonblocking(true).map_err(|error| io_error(error, "listen"))?;
        let mut bytes = [0u8; TOKEN_BYTES];
        SystemRandom::new().fill(&mut bytes).map_err(|_| start_error("cannot make a token for the Node backend"))?;
        let token = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self { listener, token })
    }

    pub fn port(&self) -> Result<u16, JsError> {
        self.listener.local_addr().map(|address| address.port()).map_err(|error| io_error(error, "listen"))
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Accept connections until one sends the token, `deadline` passes, or `exited` holds. The
    /// listener closes when this returns.
    pub fn accept(self, deadline: Instant, mut exited: impl FnMut() -> bool) -> Result<TcpStream, JsError> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(stream) = self.verify(stream, deadline) {
                        return Ok(stream);
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => return Err(io_error(error, "accept the connection")),
            }
            if exited() {
                return Err(start_error("node exited before it connected"));
            }
            if Instant::now() >= deadline {
                return Err(start_error("the Node backend did not connect in time"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn verify(&self, mut stream: TcpStream, deadline: Instant) -> Option<TcpStream> {
        let wait = deadline.saturating_duration_since(Instant::now()).clamp(Duration::from_millis(1), HELLO_TIMEOUT);
        stream.set_nonblocking(false).ok()?;
        stream.set_read_timeout(Some(wait)).ok()?;
        let line = read_line(&mut stream, self.token.len() + 1)?;
        if !same(line.as_bytes(), self.token.as_bytes()) {
            return None;
        }
        stream.set_read_timeout(None).ok()?;
        stream.set_nodelay(true).ok()?;
        Some(stream)
    }
}

/// One line of at most `limit` bytes, read a byte at a time so no frame after it is consumed.
fn read_line(stream: &mut TcpStream, limit: usize) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() <= limit {
        match stream.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => return String::from_utf8(line).ok(),
            Ok(1) => line.push(byte[0]),
            _ => return None,
        }
    }
    None
}

fn same(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// Copy the child's raw `stream` to the sink on a detached thread, which ends at end of file.
pub(crate) fn forward(
    name: String,
    output: Arc<dyn OutputSink>,
    stream: OutputStream,
    mut reader: impl Read + Send + 'static,
) -> Result<(), JsError> {
    let copy = move || {
        let mut buffer = [0u8; 8192];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 {
                break;
            }
            output.write(stream, &buffer[..count]);
        }
    };
    std::thread::Builder::new().name(name).spawn(copy).map(drop).map_err(|error| io_error(error, "start a thread"))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn connect(port: u16, hello: String) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            stream.write_all(hello.as_bytes()).unwrap();
            let mut rest = Vec::new();
            let _ = stream.read_to_end(&mut rest);
            rest
        })
    }

    #[test]
    fn a_wrong_token_is_rejected() {
        let listener = Listener::bind().unwrap();
        let port = listener.port().unwrap();
        let intruder = connect(port, "0000\n".into());
        let deadline = Instant::now() + Duration::from_millis(500);
        let error = listener.accept(deadline, || false).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("ERR_PI_NODE_START"));
        assert!(intruder.join().unwrap().is_empty());
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err(), "the listener is closed");
    }

    #[test]
    fn the_right_token_is_accepted_after_a_wrong_one() {
        let listener = Listener::bind().unwrap();
        let port = listener.port().unwrap();
        assert_eq!(listener.token().len(), TOKEN_BYTES * 2);
        let intruder = connect(port, format!("{}\n", "f".repeat(listener.token().len())));
        std::thread::sleep(Duration::from_millis(50));
        let node = connect(port, format!("{}\n", listener.token()));
        let mut stream = listener.accept(Instant::now() + Duration::from_secs(5), || false).unwrap();
        stream.write_all(b"ok").unwrap();
        drop(stream);
        assert!(intruder.join().unwrap().is_empty());
        assert_eq!(node.join().unwrap(), b"ok");
    }

    #[test]
    fn an_exited_child_stops_the_wait() {
        let listener = Listener::bind().unwrap();
        let error = listener.accept(Instant::now() + Duration::from_secs(30), || true).unwrap_err();
        assert!(error.message.contains("exited"), "{error:?}");
    }
}
