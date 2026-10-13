//! The selector header: title, scope, name filter, sort mode, and the hint or status lines.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use pi_tui::Component;
use pi_tui::ui_loop::{self, TimerId};
use pi_tui::utils::{truncate_to_width_with, visible_width};

use super::search::{NameFilter, SortMode};
use super::{RequestRender, SessionScope};
use crate::interactive_components::key_hint;
use crate::theme::{ThemeColor, theme};

/// Kind of a header status message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Error,
}

/// A transient message shown in place of the key hints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusMessage {
    pub kind: StatusKind,
    pub message: String,
}

impl StatusMessage {
    pub fn info(message: &str) -> Self {
        Self { kind: StatusKind::Info, message: message.to_string() }
    }

    pub fn error(message: &str) -> Self {
        Self { kind: StatusKind::Error, message: message.to_string() }
    }
}

/// Header of the session selector (upstream `SessionSelectorHeader`).
pub struct SessionSelectorHeader {
    scope: SessionScope,
    sort_mode: SortMode,
    name_filter: NameFilter,
    request_render: RequestRender,
    loading: bool,
    load_progress: Option<(usize, usize)>,
    show_path: bool,
    confirming_delete_path: Option<PathBuf>,
    status: Rc<RefCell<Option<StatusMessage>>>,
    status_timer: Rc<Cell<Option<TimerId>>>,
    show_rename_hint: bool,
}

impl SessionSelectorHeader {
    pub fn new(
        scope: SessionScope,
        sort_mode: SortMode,
        name_filter: NameFilter,
        request_render: RequestRender,
    ) -> Self {
        Self {
            scope,
            sort_mode,
            name_filter,
            request_render,
            loading: false,
            load_progress: None,
            show_path: false,
            confirming_delete_path: None,
            status: Rc::default(),
            status_timer: Rc::default(),
            show_rename_hint: false,
        }
    }

    pub fn set_scope(&mut self, scope: SessionScope) {
        self.scope = scope;
    }

    pub fn set_sort_mode(&mut self, sort_mode: SortMode) {
        self.sort_mode = sort_mode;
    }

    pub fn set_name_filter(&mut self, name_filter: NameFilter) {
        self.name_filter = name_filter;
    }

    /// Progress belongs to one load, so it resets whenever the loading state is set.
    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
        self.load_progress = None;
    }

    pub fn set_progress(&mut self, loaded: usize, total: usize) {
        self.load_progress = Some((loaded, total));
    }

    pub fn set_show_path(&mut self, show_path: bool) {
        self.show_path = show_path;
    }

    pub fn set_show_rename_hint(&mut self, show: bool) {
        self.show_rename_hint = show;
    }

    pub fn set_confirming_delete_path(&mut self, path: Option<PathBuf>) {
        self.confirming_delete_path = path;
    }

    pub fn status_message(&self) -> Option<StatusMessage> {
        self.status.borrow().clone()
    }

    fn clear_status_timeout(&mut self) {
        if let Some(timer) = self.status_timer.take() {
            ui_loop::clear_timer(timer);
        }
    }

    /// Shows `message` (or clears it); with `auto_hide_ms`, it clears itself and re-renders.
    pub fn set_status_message(&mut self, message: Option<StatusMessage>, auto_hide_ms: Option<u64>) {
        self.clear_status_timeout();
        let show = message.is_some();
        *self.status.borrow_mut() = message;
        let Some(delay) = auto_hide_ms.filter(|delay| show && *delay > 0) else { return };
        let (status, timer, request_render) =
            (self.status.clone(), self.status_timer.clone(), self.request_render.clone());
        let id = ui_loop::set_timeout(Duration::from_millis(delay), move || {
            status.borrow_mut().take();
            timer.set(None);
            request_render();
        });
        self.status_timer.set(Some(id));
    }

    fn hint_lines(&self, width: usize) -> (String, String) {
        let theme = theme();
        if self.confirming_delete_path.is_some() {
            let confirm = format!(
                "Delete session? {} · {}",
                key_hint("tui.select.confirm", "confirm"),
                key_hint("tui.select.cancel", "cancel")
            );
            return (theme.fg(ThemeColor::Error, &truncate_to_width_with(&confirm, width, "…", false)), String::new());
        }
        if let Some(status) = self.status.borrow().as_ref() {
            let color = if status.kind == StatusKind::Error { ThemeColor::Error } else { ThemeColor::Accent };
            return (theme.fg(color, &truncate_to_width_with(&status.message, width, "…", false)), String::new());
        }
        let path_state = if self.show_path { "(on)" } else { "(off)" };
        let sep = theme.fg(ThemeColor::Muted, " · ");
        let hint1 = key_hint("tui.input.tab", "scope")
            + &sep
            + &theme.fg(ThemeColor::Muted, "re:<pattern> regex · \"phrase\" exact");
        let mut parts = vec![
            key_hint("app.session.toggleSort", "sort"),
            key_hint("app.session.toggleNamedFilter", "named"),
            key_hint("app.session.delete", "delete"),
            key_hint("app.session.togglePath", &format!("path {path_state}")),
        ];
        if self.show_rename_hint {
            parts.push(key_hint("app.session.rename", "rename"));
        }
        let hint2 = parts.join(&sep);
        (truncate_to_width_with(&hint1, width, "…", false), truncate_to_width_with(&hint2, width, "…", false))
    }
}

impl Component for SessionSelectorHeader {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme = theme();
        let title = match self.scope {
            SessionScope::Current => "Resume Session (Current Folder)",
            SessionScope::All => "Resume Session (All)",
        };
        let left_text = theme.bold(title);
        let sort_label = match self.sort_mode {
            SortMode::Threaded => "Threaded",
            SortMode::Recent => "Recent",
            SortMode::Relevance => "Fuzzy",
        };
        let sort_text = theme.fg(ThemeColor::Muted, "Sort: ") + &theme.fg(ThemeColor::Accent, sort_label);
        let name_label = if self.name_filter == NameFilter::All { "All" } else { "Named" };
        let name_text = theme.fg(ThemeColor::Muted, "Name: ") + &theme.fg(ThemeColor::Accent, name_label);
        let scope_text = if self.loading {
            let progress = match self.load_progress {
                Some((loaded, total)) => format!("{loaded}/{total}"),
                None => "...".to_string(),
            };
            theme.fg(ThemeColor::Muted, "○ Current Folder | ")
                + &theme.fg(ThemeColor::Accent, &format!("Loading {progress}"))
        } else if self.scope == SessionScope::Current {
            theme.fg(ThemeColor::Accent, "◉ Current Folder") + &theme.fg(ThemeColor::Muted, " | ○ All")
        } else {
            theme.fg(ThemeColor::Muted, "○ Current Folder | ") + &theme.fg(ThemeColor::Accent, "◉ All")
        };
        let right = truncate_to_width_with(&format!("{scope_text}  {name_text}  {sort_text}"), width, "", false);
        let available_left = width.saturating_sub(visible_width(&right) + 1);
        let left = truncate_to_width_with(&left_text, available_left, "", false);
        let spacing = width.saturating_sub(visible_width(&left) + visible_width(&right));
        let (hint1, hint2) = self.hint_lines(width);
        vec![format!("{left}{}{right}", " ".repeat(spacing)), hint1, hint2]
    }
}
