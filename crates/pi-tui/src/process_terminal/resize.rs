//! [`ResizeWatcher`]: resize notifications for the controlling terminal.

/// Watches SIGWINCH on its own thread and runs `on_resize` for each delivery. The thread runs a
/// private Tokio runtime, so it works with or without a runtime on the caller's thread.
#[cfg(unix)]
pub struct ResizeWatcher {
    stop: tokio::sync::oneshot::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

#[cfg(unix)]
impl ResizeWatcher {
    /// Returns once the SIGWINCH handler is installed, so a signal sent right after is seen.
    pub fn spawn(mut on_resize: impl FnMut() + Send + 'static) -> Option<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pi-tui-resize".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_io().build() else {
                    return;
                };
                runtime.block_on(async move {
                    let Ok(mut resized) = signal(SignalKind::window_change()) else {
                        return;
                    };
                    let _ = ready_tx.send(());
                    loop {
                        tokio::select! {
                            biased;
                            _ = &mut stopped => break,
                            received = resized.recv() => match received {
                                Some(()) => on_resize(),
                                None => break,
                            },
                        }
                    }
                });
            })
            .ok()?;
        if ready_rx.recv().is_err() {
            let _ = thread.join();
            return None;
        }
        Some(Self { stop, thread })
    }

    /// Stops the thread and waits for it. Dropping the watcher also stops it, without waiting.
    pub fn stop(self) {
        drop(self.stop);
        let _ = self.thread.join();
    }
}

/// Windows reports resizes through console input records, so no watcher thread is needed.
#[cfg(windows)]
pub struct ResizeWatcher;

#[cfg(windows)]
impl ResizeWatcher {
    pub fn spawn(_on_resize: impl FnMut() + Send + 'static) -> Option<Self> {
        Some(Self)
    }

    pub fn stop(self) {}
}
