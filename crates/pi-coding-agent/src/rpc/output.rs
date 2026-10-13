//! Protocol output (upstream `writeRawStdout` and `waitForRawStdoutBackpressure` of
//! `core/output-guard.ts`): one writer thread owns the protocol stream and writes records in
//! order. A full queue blocks the sender, so a client that stops reading stalls the agent.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::jsonl::serialize_json_line;

/// Records that may wait for the writer before senders block.
const QUEUE_CAPACITY: usize = 256;
/// Records written per flush when the queue backs up.
const BATCH: usize = 64;
const RETRY_DELAY: Duration = Duration::from_millis(10);

struct Inner {
    sender: SyncSender<String>,
    queued: AtomicU64,
    written: watch::Receiver<u64>,
    closed: CancellationToken,
}

/// A cloneable handle that queues records for the writer thread.
#[derive(Clone)]
pub struct RpcOutput {
    inner: Arc<Inner>,
}

impl RpcOutput {
    /// Start the writer thread over `writer`.
    pub fn spawn(writer: Box<dyn Write + Send>) -> Self {
        let (sender, receiver) = sync_channel(QUEUE_CAPACITY);
        let (written_sender, written) = watch::channel(0u64);
        let closed = CancellationToken::new();
        let on_failure = closed.clone();
        std::thread::Builder::new()
            .name("pi-rpc-output".into())
            .spawn(move || write_records(writer, receiver, written_sender, on_failure))
            .expect("spawn the RPC output thread");
        Self { inner: Arc::new(Inner { sender, queued: AtomicU64::new(0), written, closed }) }
    }

    /// Queue one record. Blocks while the queue is full.
    pub fn send(&self, value: &Value) {
        self.send_line(serialize_json_line(value));
    }

    /// Queue an already serialized record, which must end with LF.
    pub fn send_line(&self, line: String) {
        self.inner.queued.fetch_add(1, Ordering::SeqCst);
        match self.inner.sender.try_send(line) {
            Ok(()) => {}
            Err(TrySendError::Full(line)) => {
                let _ = self.inner.sender.send(line);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Wait until every record queued so far is written, or the stream failed.
    pub async fn drained(&self) {
        let target = self.inner.queued.load(Ordering::SeqCst);
        let mut written = self.inner.written.clone();
        tokio::select! {
            _ = written.wait_for(|count| *count >= target) => {}
            _ = self.inner.closed.cancelled() => {}
        }
    }

    /// Cancelled when writing failed, for example because the client closed the pipe.
    pub fn closed(&self) -> CancellationToken {
        self.inner.closed.clone()
    }
}

fn write_records(
    mut writer: Box<dyn Write + Send>,
    receiver: Receiver<String>,
    written: watch::Sender<u64>,
    closed: CancellationToken,
) {
    let mut failed = false;
    while let Ok(first) = receiver.recv() {
        let mut batch = vec![first];
        while batch.len() < BATCH {
            match receiver.try_recv() {
                Ok(next) => batch.push(next),
                Err(_) => break,
            }
        }
        if !failed {
            let result = batch.iter().try_for_each(|line| write_all_retrying(&mut writer, line.as_bytes()));
            if result.and_then(|()| writer.flush()).is_err() {
                failed = true;
                closed.cancel();
            }
        }
        written.send_modify(|count| *count += batch.len() as u64);
    }
}

/// Write everything, retrying while a non-blocking pipe is full (upstream retries `EAGAIN`).
fn write_all_retrying(writer: &mut dyn Write, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match writer.write(bytes) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "output closed")),
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(RETRY_DELAY),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Reserve stdout for protocol records and return a writer for them; later calls return the
/// same stream. On Unix, descriptor 1 then points at stderr, so stray prints and inherited
/// child output stay off the protocol stream.
pub fn take_over_stdout() -> io::Result<Box<dyn Write + Send>> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        static RAW: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);
        let mut raw = RAW.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if raw.is_none() {
            io::stdout().flush()?;
            let saved = rustix::io::fcntl_dupfd_cloexec(io::stdout().as_fd(), 3)?;
            rustix::stdio::dup2_stdout(io::stderr().as_fd())?;
            *raw = Some(std::fs::File::from(saved));
        }
        let file = raw.as_ref().expect("stdout was saved").try_clone()?;
        Ok(Box::new(file))
    }
    #[cfg(not(unix))]
    {
        Ok(Box::new(io::stdout()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn writes_records_in_order_and_waits_for_them() {
        let sink = Sink::default();
        let output = RpcOutput::spawn(Box::new(sink.clone()));
        for index in 0..1000 {
            output.send(&json!({"n": index}));
        }
        output.drained().await;
        let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        let numbers: Vec<u64> =
            text.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()["n"].as_u64().unwrap()).collect();
        assert_eq!(numbers, (0..1000).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn reports_a_closed_stream_and_keeps_accepting_records() {
        let output = RpcOutput::spawn(Box::new(Broken));
        output.send(&json!({"type": "x"}));
        output.closed().cancelled().await;
        for _ in 0..(QUEUE_CAPACITY * 2) {
            output.send(&json!({"type": "y"}));
        }
        output.drained().await;
    }

    #[test]
    fn retries_would_block_writes() {
        struct Flaky(u8, Vec<u8>);
        impl Write for Flaky {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0 += 1;
                if self.0 == 1 {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
                self.1.push(buf[0]);
                Ok(1)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut flaky = Flaky(0, Vec::new());
        write_all_retrying(&mut flaky, b"ok").unwrap();
        assert_eq!(flaky.1, b"ok");
    }
}
