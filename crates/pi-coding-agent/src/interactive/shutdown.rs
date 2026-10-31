//! Ending the interactive mode on a stop signal or a fatal runtime error (upstream `shutdown`
//! with `fromSignal` and `handleFatalRuntimeError`).

use std::time::Duration;

use super::mode::InteractiveMode;
use crate::diagnostics::{
    CrashContext, ExtensionStackInfo, crash_log_path, find_extension_stack_matches, format_crash_extension_hint,
    record_fatal_error,
};
use crate::settings::schema::FullscreenExitOutput;
use crate::theme::{ThemeColor, theme};

/// The error `run_interactive` returns after a fatal runtime error was shown and recorded, so
/// the caller exits with status 1 without reporting it again.
#[derive(Debug)]
pub struct FatalErrorReported(pub String);

impl std::fmt::Display for FatalErrorReported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FatalErrorReported {}

/// How long quitting waits for the running turn and `session_shutdown` handlers to finish.
const DISPOSE_TIMEOUT: Duration = Duration::from_secs(3);

impl InteractiveMode {
    /// Dispose of the runtime, waiting at most [`DISPOSE_TIMEOUT`]. A late cleanup task keeps
    /// running detached until the process exits; entries are already on disk as they are appended.
    pub(super) async fn dispose_runtime_bounded(&self) {
        let runtime = self.runtime.clone();
        let dispose = tokio::spawn(async move { runtime.dispose().await });
        let _ = tokio::time::timeout(DISPOSE_TIMEOUT, dispose).await;
    }

    /// SIGTERM or SIGHUP: dispose of the runtime before touching the terminal, then stop the
    /// TUI. No resume hint is printed.
    pub(super) fn request_signal_shutdown(&self) {
        if std::mem::replace(&mut self.state.borrow_mut().shutting_down, true) {
            return;
        }
        self.spawn(|mode| async move {
            mode.dispose_runtime_bounded().await;
            if let Some(controller) = mode.theme_controller() {
                controller.disable_auto_sync();
            }
            mode.terminal.borrow_mut().drain_input(Duration::from_millis(1000), Duration::from_millis(50));
            mode.stop(None);
            mode.state.borrow_mut().exit = true;
            mode.bridge.waker().wake();
        });
    }

    /// Show the error, record the crash for `/bug`, stop the TUI keeping the transcript, and
    /// end the run with status 1.
    pub(super) fn handle_fatal_runtime_error(&self, prefix: &str, error: &anyhow::Error) {
        let message = format!("{prefix}: {error:#}");
        self.show_error(&message);
        if let Some(hint) = self.crash_extension_hint(error) {
            let pad = self.state.borrow().output_pad;
            self.add_text(move || theme().fg(ThemeColor::Warning, &hint), pad, 0);
        }
        let session = self.session();
        let context = CrashContext::default();
        context.set_session_file(session.session_file().as_deref());
        context.set_cwd(session.cwd());
        if let Some(instructions) = record_fatal_error(&crash_log_path(&self.agent_dir), &context, error) {
            let pad = self.state.borrow().output_pad;
            self.add_text(move || theme().fg(ThemeColor::Muted, &instructions), pad, 0);
        }
        if let Some(controller) = self.theme_controller() {
            controller.disable_auto_sync();
        }
        self.stop(Some(FullscreenExitOutput::Transcript));
        {
            let mut state = self.state.borrow_mut();
            state.shutting_down = true;
            state.exit = true;
            state.fatal_error = Some(message);
        }
        self.bridge.waker().wake();
    }
}

impl InteractiveMode {
    /// Loaded extensions whose files appear in the error's JavaScript stack, as a warning.
    fn crash_extension_hint(&self, error: &anyhow::Error) -> Option<String> {
        let entries = self.session().services().resources().resources().extensions.clone();
        let extensions: Vec<ExtensionStackInfo> = crate::extensions::load_entries(&entries)
            .into_iter()
            .map(|entry| {
                let (source, origin, base_dir) = match entry.source_info {
                    Some(info) => {
                        let package = info.origin == crate::resources::SourceOrigin::Package;
                        (info.source, if package { "package" } else { "top-level" }, info.base_dir)
                    }
                    None => (String::new(), "top-level", None),
                };
                ExtensionStackInfo {
                    path: entry.path,
                    resolved_path: entry.resolved_path.display().to_string(),
                    source,
                    origin: origin.to_string(),
                    base_dir,
                }
            })
            .collect();
        let stack = crate::diagnostics::error_stack(error);
        format_crash_extension_hint(&find_extension_stack_matches(Some(&stack), &extensions))
    }
}

/// The result of a spawned task, with a panic or cancellation as an error.
pub(super) fn joined<T>(result: Result<anyhow::Result<T>, tokio::task::JoinError>) -> anyhow::Result<T> {
    result.unwrap_or_else(|error| Err(anyhow::Error::new(error)))
}
