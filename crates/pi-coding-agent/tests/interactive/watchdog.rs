//! A stage watchdog for tests that start child processes. Shared by several test binaries.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Ends the test process when a test outlives `limit`, naming its last stage, so a hang fails in
/// minutes. Create it outside `local` so a hang while the test tears down is caught too.
pub struct Watchdog {
    stage: Arc<std::sync::Mutex<&'static str>>,
    done: Arc<AtomicBool>,
}

impl Watchdog {
    pub fn start(test: &'static str, limit: Duration) -> Self {
        let stage = Arc::new(std::sync::Mutex::new("start"));
        let done = Arc::new(AtomicBool::new(false));
        let (seen, finished) = (stage.clone(), done.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            while started.elapsed() < limit {
                if finished.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            if finished.load(Ordering::SeqCst) {
                return;
            }
            let stage = *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            // Written past the test output capture, which a process exit would discard.
            let line = format!("\n{test} hung for {limit:?} at stage `{stage}`; ending the test process\n");
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
            std::process::exit(101);
        });
        Self { stage, done }
    }

    pub fn stage(&self, name: &'static str) {
        *self.stage.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = name;
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
    }
}
