//! The panic hook records upstream's `uncaught_exception` crash kind. Its own binary, since the
//! hook is process-wide.

use pi_coding_agent::diagnostics::{CrashContext, crash_log_path, install_crash_hook, read_crash_log};

#[test]
fn panics_are_recorded_as_uncaught_exceptions() {
    let dir = tempfile::tempdir().unwrap();
    let path = crash_log_path(dir.path());
    let context = CrashContext::default();
    context.set_cwd(dir.path());
    install_crash_hook(path.clone(), context);
    let result = std::thread::spawn(|| panic!("boom from a worker")).join();
    let _ = std::panic::take_hook();
    assert!(result.is_err());
    let records = read_crash_log(&path);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].kind, "uncaught_exception");
    assert!(records[0].message.starts_with("boom from a worker at "), "{}", records[0].message);
}
