//! Newline-delimited JSON control connections over Unix sockets, shared by the coordinator, the server's
//! coordinator connection, and Session workers.

use std::path::Path;

use pi_chord::{ChordError, ChordResult};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use super::process::{MAX_CONTROL_LINE_BYTES, encode_control_line};

/// Reads JSON lines. Fails on oversized lines, invalid JSON, or a read error; `None` at end of stream.
pub struct LineReader {
    reader: BufReader<OwnedReadHalf>,
    buffer: Vec<u8>,
}

impl LineReader {
    pub fn new(read: OwnedReadHalf) -> Self {
        Self { reader: BufReader::new(read), buffer: Vec::new() }
    }

    pub async fn next(&mut self, label: &str) -> ChordResult<Option<Value>> {
        self.buffer.clear();
        loop {
            let available = self.reader.fill_buf().await.map_err(ChordError::other)?;
            if available.is_empty() {
                return Ok(None);
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(available.len(), |index| index + 1);
            self.buffer.extend_from_slice(&available[..take]);
            self.reader.consume(take);
            if self.buffer.len() > MAX_CONTROL_LINE_BYTES {
                return Err(ChordError::error(format!("{label} message is too large")));
            }
            if newline.is_some() {
                self.buffer.pop();
                return serde_json::from_slice(&self.buffer)
                    .map(Some)
                    .map_err(|_| ChordError::error(format!("{label} sent invalid JSON")));
            }
        }
    }
}

/// A queued line writer. Writes after the peer went away are dropped, as Node's destroyed sockets do.
#[derive(Clone)]
pub struct LineWriter {
    sender: mpsc::UnboundedSender<(String, Option<tokio::sync::oneshot::Sender<ChordResult<()>>>)>,
}

impl LineWriter {
    pub fn new(mut write: OwnedWriteHalf) -> Self {
        let (sender, mut receiver) =
            mpsc::unbounded_channel::<(String, Option<tokio::sync::oneshot::Sender<ChordResult<()>>>)>();
        tokio::spawn(async move {
            while let Some((line, done)) = receiver.recv().await {
                let result = write.write_all(line.as_bytes()).await.map_err(ChordError::other);
                let failed = result.is_err();
                if let Some(done) = done {
                    let _ = done.send(result);
                }
                if failed {
                    break;
                }
            }
            let _ = write.shutdown().await;
        });
        Self { sender }
    }

    /// Queue a message without waiting.
    pub fn post(&self, message: &Value) {
        if let Ok(line) = encode_control_line(message) {
            let _ = self.sender.send((line, None));
        }
    }

    /// Write a message and wait until the socket accepted it.
    pub async fn write(&self, message: &Value) -> ChordResult<()> {
        let line = encode_control_line(message)?;
        let (done, wait) = tokio::sync::oneshot::channel();
        self.sender.send((line, Some(done))).map_err(|_| ChordError::error("Control connection is closed"))?;
        wait.await.map_err(|_| ChordError::error("Control connection is closed"))?
    }

    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}

pub async fn connect(path: &Path) -> std::io::Result<(LineReader, LineWriter)> {
    let stream = UnixStream::connect(path).await?;
    let (read, write) = stream.into_split();
    Ok((LineReader::new(read), LineWriter::new(write)))
}

/// Connect, or `None` when nothing listens at `path`.
pub async fn try_connect(path: &Path) -> ChordResult<Option<UnixStream>> {
    match UnixStream::connect(path).await {
        Ok(stream) => Ok(Some(stream)),
        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
            Ok(None)
        }
        Err(error) => Err(ChordError::other(error)),
    }
}

/// SIGINT or SIGTERM.
pub struct Signals(tokio::signal::unix::Signal, tokio::signal::unix::Signal);

impl Signals {
    pub async fn recv(&mut self) {
        tokio::select! {
            _ = self.0.recv() => {}
            _ = self.1.recv() => {}
        }
    }
}

pub fn signals() -> ChordResult<Signals> {
    use tokio::signal::unix::{SignalKind, signal};
    let interrupt = signal(SignalKind::interrupt()).map_err(ChordError::other)?;
    let terminate = signal(SignalKind::terminate()).map_err(ChordError::other)?;
    Ok(Signals(interrupt, terminate))
}
