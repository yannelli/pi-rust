//! Status reporting: the working, retry, and compaction indicators, the terminal title, progress
//! (OSC 9;4), and program status (OSC 7501, upstream `program-status-reporter.ts`).

use std::rc::Rc;

use pi_ai::{Message, StopReason};
use pi_tui::components::StyleFn;
use pi_tui::program_status::{BlockedKind, ProgramState, ProgramStatus};

use super::bridge::UiEvent;
use super::components::{IdleStatus, SharedIndicator, StatusIndicator, StatusKind};
use super::mode::{DEFAULT_WORKING_MESSAGE, InteractiveMode};
use crate::agent_session::CompactionReason;
use crate::resources::brand::APP_NAME;
use crate::settings::schema::TuiMode;
use crate::theme::theme;

fn first_line(text: Option<&str>) -> String {
    let line = text.and_then(|text| text.split(['\r', '\n']).next()).map(str::trim).unwrap_or("");
    if line.is_empty() { "Error".to_string() } else { line.to_string() }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Report {
    state: ProgramState,
    kind: Option<BlockedKind>,
    message: Option<String>,
}

impl Report {
    fn new(state: ProgramState) -> Self {
        Self { state, kind: None, message: None }
    }

    fn with_message(state: ProgramState, message: String) -> Self {
        Self { state, kind: None, message: Some(message) }
    }
}

/// Working, blocked, done, and error states for terminals that support OSC 7501. Messages are
/// limited to the session name, dialog titles, and the first line of errors.
pub struct ProgramStatusReporter {
    run_active: bool,
    compacting: bool,
    run_result: Report,
    resting: Report,
    blocked: Vec<(String, BlockedKind, String)>,
    last_report: Option<ProgramStatus>,
}

impl Default for ProgramStatusReporter {
    fn default() -> Self {
        Self {
            run_active: false,
            compacting: false,
            run_result: Report::new(ProgramState::Done),
            resting: Report::new(ProgramState::Idle),
            blocked: Vec::new(),
            last_report: None,
        }
    }
}

impl ProgramStatusReporter {
    /// Update from a session event. Returns whether the status may have changed.
    pub fn handle_event(&mut self, event: &UiEvent) -> bool {
        match event {
            UiEvent::AgentStart => {
                self.run_active = true;
                self.run_result = Report::new(ProgramState::Done);
            }
            UiEvent::MessageEnd(Message::Assistant(message)) => {
                self.run_result = if message.stop_reason == StopReason::Error {
                    Report::with_message(ProgramState::Error, first_line(message.error_message.as_deref()))
                } else {
                    Report::new(ProgramState::Done)
                };
            }
            UiEvent::CompactionStart { .. } => self.compacting = true,
            UiEvent::CompactionEnd { reason, aborted, error_message, .. } => {
                self.compacting = false;
                let error = error_message.as_deref().map(|message| first_line(Some(message)));
                if self.run_active {
                    if *aborted {
                        self.run_result = Report::new(ProgramState::Idle);
                    } else if let Some(error) = error {
                        self.run_result = Report::with_message(ProgramState::Error, error);
                    }
                } else if *aborted {
                    self.resting = Report::new(ProgramState::Idle);
                } else if *reason == CompactionReason::Manual {
                    self.resting = match error {
                        Some(error) => Report::with_message(ProgramState::Error, error),
                        None => Report::new(ProgramState::Done),
                    };
                }
            }
            UiEvent::AgentSettled { aborted } => {
                self.run_active = false;
                self.resting = if *aborted { Report::new(ProgramState::Idle) } else { self.run_result.clone() };
            }
            UiEvent::SessionInfoChanged => {}
            _ => return false,
        }
        true
    }

    /// Report `blocked` for a dialog until it is cleared with `None`. Reopening a source replaces it.
    pub fn set_blocked(&mut self, source: &str, status: Option<(BlockedKind, String)>) {
        self.blocked.retain(|(existing, _, _)| existing != source);
        if let Some((kind, message)) = status {
            self.blocked.push((source.to_string(), kind, message));
        }
    }

    /// Forget the replaced session's run.
    pub fn reset(&mut self) {
        *self = Self { blocked: std::mem::take(&mut self.blocked), last_report: self.last_report.take(), ..Self::default() };
    }

    /// The status to show for `session_name`.
    pub fn status(&self, session_name: Option<String>) -> ProgramStatus {
        let report = if let Some((_, kind, message)) = self.blocked.last() {
            Report { state: ProgramState::Blocked, kind: Some(*kind), message: Some(message.clone()) }
        } else if self.compacting {
            Report::with_message(ProgramState::Working, "Compacting context".to_string())
        } else if self.run_active {
            Report { state: ProgramState::Working, kind: None, message: session_name }
        } else if self.resting.state == ProgramState::Done {
            Report { message: session_name, ..self.resting.clone() }
        } else {
            self.resting.clone()
        };
        ProgramStatus { state: report.state, app: Some(APP_NAME.to_string()), kind: report.kind, message: report.message }
    }

    /// The status when it differs from the last one reported.
    pub fn take_report(&mut self, session_name: Option<String>) -> Option<ProgramStatus> {
        let status = self.status(session_name);
        if self.last_report.as_ref() == Some(&status) {
            return None;
        }
        self.last_report = Some(status.clone());
        Some(status)
    }
}

impl InteractiveMode {
    pub(super) fn report_program_status(&self) {
        let name = self.session().session_name();
        let status = self.program_status.borrow_mut().take_report(name);
        if let Some(status) = status {
            self.terminal.borrow_mut().set_program_status(&status);
        }
    }

    /// Mark a dialog as waiting for the user (OSC 7501 `blocked`), or clear it.
    pub(super) fn set_blocked_status(&self, source: &str, status: Option<(BlockedKind, String)>) {
        self.program_status.borrow_mut().set_blocked(source, status);
        self.report_program_status();
    }

    pub(super) fn set_terminal_progress(&self, active: bool) {
        if self.settings().show_terminal_progress() {
            self.terminal.borrow_mut().set_progress(active);
        }
    }

    /// `π - <session name> - <cwd name>` (upstream `updateTerminalTitle`).
    pub(super) fn update_terminal_title(&self) {
        let session = self.session();
        let cwd = session.cwd().file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
        let app = if APP_NAME == "pi" { "π" } else { APP_NAME };
        let title = match session.session_name() {
            Some(name) => format!("{app} - {name} - {cwd}"),
            None => format!("{app} - {cwd}"),
        };
        self.terminal.borrow_mut().set_title(&title);
    }

    /// Show `indicator` in the editor border, replacing the active one.
    pub(super) fn show_status_indicator(&self, indicator: SharedIndicator) {
        let previous = {
            let mut state = self.state.borrow_mut();
            state.active_status_embedded = false;
            state.active_status.replace(indicator.clone())
        };
        if let Some(previous) = previous {
            previous.borrow_mut().dispose();
        }
        self.ui.status.borrow_mut().clear();
        self.editor.set_working_status_indicator(None);
        if self.editor.embed_working_status() {
            self.editor.set_working_status_indicator(Some(indicator));
            self.state.borrow_mut().active_status_embedded = true;
        } else {
            self.ui.status.borrow_mut().add_child(indicator);
        }
        self.request_render();
    }

    /// Remove the active indicator, or only one of `kind`.
    pub(super) fn clear_status_indicator(&self, kind: Option<StatusKind>) {
        let (cleared, embedded) = {
            let mut state = self.state.borrow_mut();
            let matches = state.active_status.as_ref().is_some_and(|active| {
                kind.is_none_or(|kind| active.try_borrow().is_ok_and(|active| active.kind() == kind))
            });
            if kind.is_some() && !matches {
                return;
            }
            let embedded = std::mem::take(&mut state.active_status_embedded);
            (state.active_status.take(), embedded)
        };
        if let Some(indicator) = &cleared {
            indicator.borrow_mut().dispose();
        }
        self.ui.status.borrow_mut().clear();
        self.editor.set_working_status_indicator(None);
        if cleared.is_some() && !embedded && self.tui_mode() == TuiMode::Regular && self.tui().get_clear_on_shrink() {
            self.ui.status.borrow_mut().add_child(pi_tui::component(IdleStatus));
        }
        self.request_render();
    }

    pub(super) fn active_status_kind(&self) -> Option<StatusKind> {
        let state = self.state.borrow();
        state.active_status.as_ref().and_then(|active| active.try_borrow().ok().map(|active| active.kind()))
    }

    /// The spinner for an agent run, colored like the editor border.
    pub(super) fn show_working_status_indicator(&self) {
        let color: Option<StyleFn> = self.editor.embed_working_status().then(|| {
            let editor = self.editor.clone();
            Rc::new(move |text: &str| (editor.border_color())(text)) as StyleFn
        });
        let indicator = StatusIndicator::working(&self.tui(), DEFAULT_WORKING_MESSAGE, None, color);
        self.show_status_indicator(indicator);
    }

    pub(super) fn show_compaction_indicator(&self, reason: CompactionReason) {
        self.show_status_indicator(StatusIndicator::compaction(&self.tui(), reason));
    }

    /// Editor border: the bash color in `!` mode, otherwise the thinking level color.
    pub(super) fn update_editor_border_color(&self) {
        let bash_mode = self.state.borrow().is_bash_mode;
        let style: StyleFn = if bash_mode {
            Rc::new(|text: &str| theme().get_bash_mode_border_color()(text))
        } else {
            let level = self.session().thinking_level();
            Rc::new(move |text: &str| theme().get_thinking_border_color(level)(text))
        };
        self.editor.set_border_color(style);
        let active = self.state.borrow().active_status.clone();
        if let Some(active) = active
            && let Ok(mut active) = active.try_borrow_mut()
        {
            pi_tui::Component::invalidate(&mut *active);
        }
        self.request_render();
    }
}
