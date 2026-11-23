//! Port of upstream `test/regression-sigwinch-kill-eacces.test.ts`. Upstream replaces
//! `process.kill` and `process.platform`; here `refresh_terminal_dimensions_with` takes both.

use std::cell::Cell;
use std::io;

use pi_tui::terminal::{refresh_terminal_dimensions, refresh_terminal_dimensions_with};

#[test]
fn does_not_throw_when_kill_2_returns_eacces_for_self_signal() {
    let called = Cell::new(false);
    refresh_terminal_dimensions_with(false, || {
        called.set(true);
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "kill EACCES"))
    });
    assert!(called.get(), "the self-signal should be attempted on linux");
}

#[test]
fn does_not_call_kill_on_win32() {
    let kill_called = Cell::new(false);
    refresh_terminal_dimensions_with(true, || {
        kill_called.set(true);
        Ok(())
    });
    assert!(!kill_called.get(), "kill should not be called on win32");
}

#[test]
fn preserves_other_error_codes() {
    // EPERM is also ignored - the refresh is best-effort
    let called = Cell::new(false);
    refresh_terminal_dimensions_with(false, || {
        called.set(true);
        Err(io::Error::other("kill EPERM"))
    });
    assert!(called.get());
}

/// Not an upstream case: the real refresh delivers SIGWINCH to this process.
#[cfg(unix)]
#[test]
fn sends_sigwinch_to_this_process() {
    use std::time::Duration;

    use tokio::signal::unix::{SignalKind, signal};

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let mut resized = signal(SignalKind::window_change()).unwrap();
        refresh_terminal_dimensions();
        tokio::time::timeout(Duration::from_secs(5), resized.recv()).await.expect("SIGWINCH within 5s");
    });
}

#[cfg(not(unix))]
#[test]
fn refresh_is_a_no_op_off_unix() {
    refresh_terminal_dimensions();
}
