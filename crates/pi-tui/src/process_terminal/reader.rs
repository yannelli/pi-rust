//! Background threads that read raw stdin and watch for resizes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::input::Core;
use crate::ui_loop::TerminalSink;

const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Incremental UTF-8 decoder: holds incomplete sequences across chunks and replaces invalid
/// bytes with U+FFFD, like Node's string decoder.
#[derive(Default)]
pub struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        let mut rest: &[u8] = &self.pending;
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    rest = &[];
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    out.push_str(std::str::from_utf8(&rest[..valid]).unwrap_or_default());
                    match error.error_len() {
                        Some(len) => {
                            out.push('\u{FFFD}');
                            rest = &rest[valid + len..];
                        }
                        None => {
                            rest = &rest[valid..];
                            break;
                        }
                    }
                }
            }
        }
        self.pending = rest.to_vec();
        out
    }
}

/// A running input thread. Dropping it does not stop the thread; call [`ReaderThread::stop`].
pub struct ReaderThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ReaderThread {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn wait_timeout(core: &Arc<Mutex<Core>>) -> Duration {
    let deadline = core.lock().ok().and_then(|core| core.next_deadline());
    match deadline {
        Some(deadline) => deadline.saturating_duration_since(Instant::now()).min(POLL_INTERVAL),
        None => POLL_INTERVAL,
    }
}

fn poll_timers(core: &Arc<Mutex<Core>>) {
    if let Ok(mut core) = core.lock() {
        core.poll_timers(Instant::now());
    }
}

/// Starts the thread that reads stdin and feeds [`Core`].
#[cfg(unix)]
pub fn spawn_reader(core: Arc<Mutex<Core>>, _sink: TerminalSink) -> ReaderThread {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};

    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let handle = std::thread::Builder::new()
        .name("pi-tui-input".into())
        .spawn(move || {
            let mut decoder = Utf8Decoder::default();
            let mut buf = [0u8; 4096];
            while !stop_flag.load(Ordering::SeqCst) {
                let timeout = wait_timeout(&core);
                let timespec = Timespec { tv_sec: timeout.as_secs() as _, tv_nsec: timeout.subsec_nanos() as _ };
                let stdin = rustix::stdio::stdin();
                let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
                let ready = matches!(poll(&mut fds, Some(&timespec)), Ok(n) if n > 0);
                if ready && !stop_flag.load(Ordering::SeqCst) {
                    match rustix::io::read(stdin, &mut buf) {
                        Ok(0) => std::thread::sleep(POLL_INTERVAL),
                        Ok(n) => {
                            let text = decoder.decode(&buf[..n]);
                            if !text.is_empty()
                                && let Ok(mut core) = core.lock()
                            {
                                core.feed(&text, Instant::now());
                            }
                        }
                        Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {}
                        Err(errno) => {
                            super::report_terminal_error(&std::io::Error::from(errno));
                            std::thread::sleep(POLL_INTERVAL);
                        }
                    }
                }
                poll_timers(&core);
            }
        })
        .expect("spawn input thread");
    ReaderThread { stop, handle: Some(handle) }
}

/// Starts the thread that reads console input records, forwards VT text, and reports resizes.
#[cfg(windows)]
pub fn spawn_reader(core: Arc<Mutex<Core>>, sink: TerminalSink) -> ReaderThread {
    use crossterm_winapi::{Console, Handle, InputRecord};

    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let handle = std::thread::Builder::new()
        .name("pi-tui-input".into())
        .spawn(move || {
            let Ok(console) = Handle::current_in_handle().map(Console::from) else { return };
            let mut high_surrogate: Option<u16> = None;
            let mut last_size = crossterm::terminal::size().ok();
            let mut last_size_check = Instant::now();
            while !stop_flag.load(Ordering::SeqCst) {
                if last_size_check.elapsed() >= Duration::from_millis(250) {
                    last_size_check = Instant::now();
                    let size = crossterm::terminal::size().ok();
                    if size != last_size {
                        last_size = size;
                        sink.resize();
                    }
                }
                let available = console.number_of_console_input_events().unwrap_or(0);
                if available == 0 {
                    std::thread::sleep(wait_timeout(&core).max(Duration::from_millis(5)));
                    poll_timers(&core);
                    continue;
                }
                let Ok(records) = console.read_console_input() else { continue };
                let mut units: Vec<u16> = Vec::new();
                for record in records {
                    match record {
                        InputRecord::KeyEvent(key) if key.key_down && key.u_char != 0 => {
                            for _ in 0..key.repeat_count.max(1) {
                                units.push(key.u_char);
                            }
                        }
                        InputRecord::WindowBufferSizeEvent(_) => sink.resize(),
                        _ => {}
                    }
                }
                let text = decode_utf16_units(&mut high_surrogate, &units);
                if !text.is_empty()
                    && let Ok(mut core) = core.lock()
                {
                    core.feed(&text, Instant::now());
                }
                poll_timers(&core);
            }
        })
        .expect("spawn input thread");
    ReaderThread { stop, handle: Some(handle) }
}

/// Decodes UTF-16 code units, keeping a trailing high surrogate for the next call.
pub fn decode_utf16_units(high_surrogate: &mut Option<u16>, units: &[u16]) -> String {
    let mut all: Vec<u16> = high_surrogate.take().into_iter().collect();
    all.extend_from_slice(units);
    if let Some(&last) = all.last()
        && (0xD800..=0xDBFF).contains(&last)
    {
        *high_surrogate = all.pop();
    }
    char::decode_utf16(all).map(|c| c.unwrap_or('\u{FFFD}')).collect()
}

/// Watches SIGWINCH and forwards resize notifications.
#[cfg(unix)]
pub struct ResizeWatcher {
    handle: signal_hook::iterator::Handle,
    thread: Option<JoinHandle<()>>,
}

#[cfg(unix)]
impl ResizeWatcher {
    pub fn spawn(sink: TerminalSink) -> Option<Self> {
        let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH]).ok()?;
        let handle = signals.handle();
        let thread = std::thread::Builder::new()
            .name("pi-tui-resize".into())
            .spawn(move || {
                for _ in signals.forever() {
                    sink.resize();
                }
            })
            .ok()?;
        Some(Self { handle, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Windows reports resizes through console input records, so no watcher thread is needed.
#[cfg(windows)]
pub struct ResizeWatcher;

#[cfg(windows)]
impl ResizeWatcher {
    pub fn spawn(_sink: TerminalSink) -> Option<Self> {
        Some(Self)
    }

    pub fn stop(self) {}
}
