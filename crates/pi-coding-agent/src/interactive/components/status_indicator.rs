//! Working, retry, compaction, and branch-summary indicators shown above the editor or embedded
//! in its top border (upstream `components/status-indicator.ts`).

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::Duration;

use pi_tui::components::{LoaderIndicatorOptions, StyleFn};
use pi_tui::utils::truncate_to_width_with;
use pi_tui::{Component, Loader, Tui};

use crate::agent_session::CompactionReason;
use crate::interactive_components::{CountdownTimer, key_text};
use crate::theme::{ThemeColor, theme};

/// Which operation an indicator reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    Working,
    Retry,
    Compaction,
    BranchSummary,
}

/// An indicator shared by the status container and the editor border.
pub type SharedIndicator = Rc<RefCell<StatusIndicator>>;

fn style(color: ThemeColor) -> StyleFn {
    Rc::new(move |text: &str| theme().fg(color, text))
}

/// A spinner and message (a [`Loader`]) tagged with its kind.
pub struct StatusIndicator {
    kind: StatusKind,
    loader: Loader,
    countdown: Option<CountdownTimer>,
}

impl StatusIndicator {
    fn shared(
        kind: StatusKind,
        tui: &Tui,
        spinner: StyleFn,
        message_style: StyleFn,
        message: &str,
        indicator: Option<LoaderIndicatorOptions>,
    ) -> SharedIndicator {
        let loader = Loader::new(Some(tui), spinner, message_style, message, indicator);
        Rc::new(RefCell::new(Self { kind, loader, countdown: None }))
    }

    /// The agent is working. `color` styles both spinner and message (the editor border color).
    pub fn working(
        tui: &Tui,
        message: &str,
        indicator: Option<LoaderIndicatorOptions>,
        color: Option<StyleFn>,
    ) -> SharedIndicator {
        let spinner = color.clone().unwrap_or_else(|| style(ThemeColor::Accent));
        let text = color.unwrap_or_else(|| style(ThemeColor::Muted));
        Self::shared(StatusKind::Working, tui, spinner, text, message, indicator)
    }

    /// A retry after `delay_ms`, counting down every second.
    pub fn retry(tui: &Tui, attempt: u32, max_attempts: u32, delay_ms: u64) -> SharedIndicator {
        let message = move |seconds: u64| {
            format!("Retrying ({attempt}/{max_attempts}) in {seconds}s... ({} to cancel)", key_text("app.interrupt"))
        };
        let initial = message(delay_ms.div_ceil(1000));
        let shared =
            Self::shared(StatusKind::Retry, tui, style(ThemeColor::Warning), style(ThemeColor::Muted), &initial, None);
        let weak: Weak<RefCell<StatusIndicator>> = Rc::downgrade(&shared);
        let countdown = CountdownTimer::new(
            Duration::from_millis(delay_ms),
            Some(tui.clone()),
            move |seconds| {
                if let Some(indicator) = weak.upgrade()
                    && let Ok(mut indicator) = indicator.try_borrow_mut()
                {
                    indicator.loader.set_message(&message(seconds));
                }
            },
            || {},
        );
        shared.borrow_mut().countdown = Some(countdown);
        shared
    }

    /// Manual or automatic compaction.
    pub fn compaction(tui: &Tui, reason: CompactionReason) -> SharedIndicator {
        let cancel = format!("({} to cancel)", key_text("app.interrupt"));
        let label = match reason {
            CompactionReason::Manual => format!("Compacting context... {cancel}"),
            CompactionReason::Overflow => format!("Context overflow detected, Auto-compacting... {cancel}"),
            CompactionReason::Threshold => format!("Auto-compacting... {cancel}"),
        };
        Self::shared(StatusKind::Compaction, tui, style(ThemeColor::Accent), style(ThemeColor::Muted), &label, None)
    }

    /// Summarizing the abandoned branch during tree navigation.
    pub fn branch_summary(tui: &Tui) -> SharedIndicator {
        let label = format!("Summarizing branch... ({} to cancel)", key_text("app.interrupt"));
        Self::shared(StatusKind::BranchSummary, tui, style(ThemeColor::Accent), style(ThemeColor::Muted), &label, None)
    }

    pub fn kind(&self) -> StatusKind {
        self.kind
    }

    pub fn message(&self) -> String {
        self.loader.message()
    }

    pub fn set_message(&mut self, message: &str) {
        self.loader.set_message(message);
    }

    pub fn set_indicator(&mut self, indicator: Option<LoaderIndicatorOptions>) {
        self.loader.set_indicator(indicator);
    }

    /// The status line without its leading padding, cut to `width`.
    pub fn render_in_border(&mut self, width: usize) -> String {
        let line = self.loader.render(width + 2).get(1).cloned().unwrap_or_default();
        let line = line.strip_prefix(' ').unwrap_or(&line).trim_end().to_string();
        truncate_to_width_with(&line, width, "", false)
    }

    /// Only the spinner frame, for borders too narrow for the message.
    pub fn render_spinner_in_border(&self, width: usize) -> String {
        truncate_to_width_with(&self.loader.rendered_indicator(), width, "", false)
    }

    /// Stop the animation and any countdown.
    pub fn dispose(&mut self) {
        if let Some(countdown) = self.countdown.take() {
            countdown.dispose();
        }
        self.loader.stop();
    }
}

impl Component for StatusIndicator {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.loader.render(width)
    }

    fn invalidate(&mut self) {
        self.loader.invalidate();
    }
}

/// Two blank lines that keep the status area height while idle in regular mode.
pub struct IdleStatus;

impl Component for IdleStatus {
    fn render(&mut self, width: usize) -> Vec<String> {
        vec![" ".repeat(width); 2]
    }
}
