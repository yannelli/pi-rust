//! Stdout guard for JSON and RPC modes (upstream `core/output-guard.ts`). While taken over, only
//! [`write_raw_stdout`] reaches stdout. On Unix the descriptor is redirected to stderr, which also
//! catches stray `println!`; on Windows other output must go through [`guarded_stdout`].

use std::io::{self, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static TAKEN_OVER: AtomicBool = AtomicBool::new(false);
static RAW: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Send stdout writes to stderr until [`restore_stdout`].
pub fn take_over_stdout() -> io::Result<()> {
    let mut raw = RAW.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if TAKEN_OVER.load(Ordering::SeqCst) {
        return Ok(());
    }
    io::stdout().flush()?;
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        let saved = rustix::io::dup(io::stdout().as_fd())?;
        rustix::stdio::dup2_stdout(io::stderr().as_fd())?;
        *raw = Some(std::fs::File::from(saved));
    }
    #[cfg(not(unix))]
    let _ = &mut raw;
    TAKEN_OVER.store(true, Ordering::SeqCst);
    Ok(())
}

/// Give stdout back.
pub fn restore_stdout() -> io::Result<()> {
    let mut raw = RAW.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !TAKEN_OVER.load(Ordering::SeqCst) {
        return Ok(());
    }
    io::stdout().flush()?;
    #[cfg(unix)]
    if let Some(saved) = raw.take() {
        use std::os::fd::AsFd;
        rustix::stdio::dup2_stdout(saved.as_fd())?;
    }
    #[cfg(not(unix))]
    let _ = raw.take();
    TAKEN_OVER.store(false, Ordering::SeqCst);
    Ok(())
}

pub fn is_stdout_taken_over() -> bool {
    TAKEN_OVER.load(Ordering::SeqCst)
}

fn write_all_retrying(writer: &mut dyn Write, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match writer.write(bytes) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "stdout closed")),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if matches!(error.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
    writer.flush()
}

/// Write protocol output to the real stdout, in order, retrying while the pipe is full.
pub fn write_raw_stdout(text: &str) -> io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let mut raw = RAW.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match raw.as_mut() {
        Some(file) => write_all_retrying(file, text.as_bytes()),
        None => write_all_retrying(&mut io::stdout().lock(), text.as_bytes()),
    }
}

/// Flush pending protocol output.
pub fn flush_raw_stdout() -> io::Result<()> {
    let mut raw = RAW.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match raw.as_mut() {
        Some(file) => file.flush(),
        None => io::stdout().flush(),
    }
}

/// Where non-protocol output goes: stderr while taken over, else stdout.
pub fn guarded_stdout() -> Box<dyn Write + Send> {
    if is_stdout_taken_over() { Box::new(io::stderr()) } else { Box::new(io::stdout()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_over_and_restore_round_trip() {
        assert!(!is_stdout_taken_over());
        write_raw_stdout("").unwrap();
        take_over_stdout().unwrap();
        take_over_stdout().unwrap();
        assert!(is_stdout_taken_over());
        write_raw_stdout("").unwrap();
        flush_raw_stdout().unwrap();
        let mut writer = guarded_stdout();
        writer.write_all(b"").unwrap();
        restore_stdout().unwrap();
        restore_stdout().unwrap();
        assert!(!is_stdout_taken_over());
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
