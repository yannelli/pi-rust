//! The session list: search input, threaded or flat rows, and the list key bindings.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use pi_tui::components::Input;
use pi_tui::utils::{truncate_to_width, truncate_to_width_with, visible_width};
use pi_tui::{Component, get_keybindings};

use super::search::{NameFilter, SortMode, ThreadedSession, filter_and_sort_sessions, threaded_sessions};
use crate::interactive_components::key_text;
use crate::session::SessionInfo;
use crate::theme::{ThemeBg, ThemeColor, theme};

/// `~` for the home directory prefix.
pub(super) fn shorten_path(path: &str) -> String {
    let home = crate::config::home_dir().display().to_string();
    match path.strip_prefix(home.as_str()) {
        Some(rest) if !path.is_empty() && !home.is_empty() => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// Age of a session's last activity (in milliseconds) at `now`: `now`, `5m`, `3h`, `2d`, `1w`, `4mo`, `1y`.
pub fn format_session_age(activity: i64, now: i64) -> String {
    let diff = now - activity;
    let (minutes, hours, days) = (diff.div_euclid(60_000), diff.div_euclid(3_600_000), diff.div_euclid(86_400_000));
    match () {
        _ if minutes < 1 => "now".to_string(),
        _ if minutes < 60 => format!("{minutes}m"),
        _ if hours < 24 => format!("{hours}h"),
        _ if days < 7 => format!("{days}d"),
        _ if days < 30 => format!("{}w", days / 7),
        _ if days < 365 => format!("{}mo", days / 30),
        _ => format!("{}y", days / 365),
    }
}

fn now_millis() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|duration| duration.as_millis() as i64).unwrap_or_default()
}

/// Upstream `canonicalizePath`: the real path, or the path itself when it cannot be resolved.
pub(super) fn canonicalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn flat(session: SessionInfo) -> ThreadedSession {
    ThreadedSession { session, depth: 0, is_last: true, ancestor_continues: Vec::new() }
}

fn tree_prefix(node: &ThreadedSession) -> String {
    if node.depth == 0 {
        return String::new();
    }
    let parts: String =
        node.ancestor_continues.iter().map(|continues| if *continues { "│  " } else { "   " }).collect();
    parts + if node.is_last { "└─ " } else { "├─ " }
}

type Callback = Option<Box<dyn FnMut()>>;

/// Session rows with a search input (upstream `SessionList`). The selector sets every callback;
/// replacing one takes over that action.
pub struct SessionList {
    all_sessions: Vec<SessionInfo>,
    filtered: Vec<ThreadedSession>,
    selected_index: usize,
    selection_touched: bool,
    search_input: Input,
    submitted: Rc<Cell<bool>>,
    show_cwd: bool,
    sort_mode: SortMode,
    name_filter: NameFilter,
    show_path: bool,
    confirming_delete_path: Option<PathBuf>,
    current_session_canonical_path: Option<PathBuf>,
    max_visible: usize,
    pub on_select: Option<Box<dyn FnMut(PathBuf)>>,
    pub on_cancel: Callback,
    pub on_exit: Callback,
    pub on_toggle_scope: Callback,
    pub on_toggle_sort: Callback,
    pub on_toggle_name_filter: Callback,
    pub on_toggle_path: Option<Box<dyn FnMut(bool)>>,
    pub on_delete_confirmation_change: Option<Box<dyn FnMut(Option<PathBuf>)>>,
    pub on_delete_session: Option<Box<dyn FnMut(PathBuf)>>,
    pub on_rename_session: Option<Box<dyn FnMut(PathBuf)>>,
    pub on_error: Option<Box<dyn FnMut(String)>>,
}

impl SessionList {
    pub fn new(
        sessions: Vec<SessionInfo>,
        show_cwd: bool,
        sort_mode: SortMode,
        name_filter: NameFilter,
        current_session_file_path: Option<&Path>,
    ) -> Self {
        let submitted = Rc::new(Cell::new(false));
        let mut search_input = Input::default();
        let flag = submitted.clone();
        search_input.set_on_submit(move |_| flag.set(true));
        let mut list = Self {
            all_sessions: sessions,
            filtered: Vec::new(),
            selected_index: 0,
            selection_touched: false,
            search_input,
            submitted,
            show_cwd,
            sort_mode,
            name_filter,
            show_path: false,
            confirming_delete_path: None,
            current_session_canonical_path: current_session_file_path.map(canonicalize),
            max_visible: 10,
            on_select: None,
            on_cancel: None,
            on_exit: None,
            on_toggle_scope: None,
            on_toggle_sort: None,
            on_toggle_name_filter: None,
            on_toggle_path: None,
            on_delete_confirmation_change: None,
            on_delete_session: None,
            on_rename_session: None,
            on_error: None,
        };
        list.filter_sessions("");
        list
    }

    pub fn selected_session_path(&self) -> Option<PathBuf> {
        self.filtered.get(self.selected_index).map(|node| node.session.path.clone())
    }

    pub fn search_query(&self) -> &str {
        self.search_input.get_value()
    }

    pub fn set_sort_mode(&mut self, sort_mode: SortMode) {
        self.sort_mode = sort_mode;
        self.refilter();
    }

    pub fn set_name_filter(&mut self, name_filter: NameFilter) {
        self.name_filter = name_filter;
        self.refilter();
    }

    /// Replaces the sessions; keeps a selection the user moved, else selects the first row.
    pub fn set_sessions(&mut self, sessions: Vec<SessionInfo>, show_cwd: bool) {
        let selected_path = if self.selection_touched { self.selected_session_path() } else { None };
        self.all_sessions = sessions;
        self.show_cwd = show_cwd;
        self.refilter();
        if !self.selection_touched {
            self.selected_index = 0;
        } else if let Some(path) = selected_path
            && let Some(index) = self.filtered.iter().position(|node| node.session.path == path)
        {
            self.selected_index = index;
        }
    }

    fn refilter(&mut self) {
        let query = self.search_input.get_value().to_string();
        self.filter_sessions(&query);
    }

    fn filter_sessions(&mut self, query: &str) {
        let named: Vec<SessionInfo> = if self.name_filter == NameFilter::All {
            self.all_sessions.clone()
        } else {
            filter_and_sort_sessions(&self.all_sessions, "", SortMode::Recent, NameFilter::Named)
        };
        self.filtered = if self.sort_mode == SortMode::Threaded && query.trim().is_empty() {
            threaded_sessions(&named)
        } else {
            filter_and_sort_sessions(&named, query, self.sort_mode, NameFilter::All).into_iter().map(flat).collect()
        };
        self.selected_index = self.selected_index.min(self.filtered.len().saturating_sub(1));
    }

    fn set_confirming_delete_path(&mut self, path: Option<PathBuf>) {
        self.confirming_delete_path = path.clone();
        if let Some(callback) = self.on_delete_confirmation_change.as_mut() {
            callback(path);
        }
    }

    fn start_delete_confirmation_for_selected_session(&mut self) {
        let Some(path) = self.selected_session_path() else { return };
        if self.is_current_session_path(&path) {
            if let Some(on_error) = self.on_error.as_mut() {
                on_error("Cannot delete the currently active session".to_string());
            }
            return;
        }
        self.set_confirming_delete_path(Some(path));
    }

    fn is_current_session_path(&self, path: &Path) -> bool {
        self.current_session_canonical_path.as_deref().is_some_and(|current| canonicalize(path) == current)
    }

    fn empty_message(&self) -> String {
        if self.name_filter == NameFilter::Named {
            let toggle = key_text("app.session.toggleNamedFilter");
            if self.show_cwd {
                format!("  No named sessions found. Press {toggle} to show all.")
            } else {
                format!("  No named sessions in current folder. Press {toggle} to show all, or Tab to view all.")
            }
        } else if self.show_cwd {
            "  No sessions found".to_string()
        } else {
            "  No sessions in current folder. Press Tab to view all.".to_string()
        }
    }

    fn render_row(&self, index: usize, width: usize, now: i64) -> String {
        let theme = theme();
        let node = &self.filtered[index];
        let session = &node.session;
        let selected = index == self.selected_index;
        let confirming = self.confirming_delete_path.as_deref() == Some(session.path.as_path());
        let prefix = tree_prefix(node);
        let has_name = session.name.as_deref().is_some_and(|name| !name.is_empty());
        let normalized: String =
            session.display_name().chars().map(|ch| if ch.is_ascii_control() { ' ' } else { ch }).collect();
        let mut right = format!("{} {}", session.message_count, format_session_age(session.modified, now));
        if self.show_cwd && !session.cwd.is_empty() {
            right = format!("{} {right}", shorten_path(&session.cwd));
        }
        if self.show_path {
            right = format!("{} {right}", shorten_path(&session.path.display().to_string()));
        }
        let cursor = if selected { theme.fg(ThemeColor::Accent, "› ") } else { "  ".to_string() };
        let available = width as i64 - 2 - visible_width(&prefix) as i64 - (visible_width(&right) + 2) as i64;
        let truncated = truncate_to_width_with(normalized.trim(), available.max(10) as usize, "…", false);
        let color = if confirming {
            Some(ThemeColor::Error)
        } else if self.is_current_session_path(&session.path) {
            Some(ThemeColor::Accent)
        } else if has_name {
            Some(ThemeColor::Warning)
        } else {
            None
        };
        let mut message = color.map(|color| theme.fg(color, &truncated)).unwrap_or(truncated);
        if selected {
            message = theme.bold(&message);
        }
        let left = cursor + &theme.fg(ThemeColor::Dim, &prefix) + &message;
        let spacing = (width as i64 - visible_width(&left) as i64 - visible_width(&right) as i64).max(1) as usize;
        let right = theme.fg(if confirming { ThemeColor::Error } else { ThemeColor::Dim }, &right);
        let mut line = format!("{left}{}{right}", " ".repeat(spacing));
        if selected {
            line = theme.bg(ThemeBg::SelectedBg, &line);
        }
        truncate_to_width(&line, width)
    }

    fn pass_to_search(&mut self, data: &str) {
        self.search_input.handle_input(data);
        if self.submitted.replace(false) {
            let selected = self.selected_session_path();
            if let (Some(path), Some(on_select)) = (selected, self.on_select.as_mut()) {
                on_select(path);
            }
        }
        self.refilter();
    }

    fn call(callback: &mut Callback) {
        if let Some(callback) = callback.as_mut() {
            callback();
        }
    }
}

impl Component for SessionList {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = self.search_input.render(width);
        lines.push(String::new());
        if self.filtered.is_empty() {
            let message = truncate_to_width_with(&self.empty_message(), width, "…", false);
            lines.push(theme().fg(ThemeColor::Muted, &message));
            return lines;
        }
        let len = self.filtered.len();
        let start = (self.selected_index as i64 - (self.max_visible / 2) as i64)
            .min(len as i64 - self.max_visible as i64)
            .max(0) as usize;
        let end = (start + self.max_visible).min(len);
        let now = now_millis();
        lines.extend((start..end).map(|index| self.render_row(index, width, now)));
        if start > 0 || end < len {
            let scroll = format!("  ({}/{len})", self.selected_index + 1);
            lines.push(theme().fg(ThemeColor::Muted, &truncate_to_width_with(&scroll, width, "", false)));
        }
        lines
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        if let Some(path) = self.confirming_delete_path.clone() {
            if kb.matches(data, "tui.select.confirm") {
                self.set_confirming_delete_path(None);
                if let Some(on_delete) = self.on_delete_session.as_mut() {
                    on_delete(path);
                }
            } else if kb.matches(data, "tui.select.cancel") {
                self.set_confirming_delete_path(None);
            }
            return;
        }
        if kb.matches(data, "tui.input.tab") {
            return Self::call(&mut self.on_toggle_scope);
        }
        if kb.matches(data, "app.session.toggleSort") {
            return Self::call(&mut self.on_toggle_sort);
        }
        if kb.matches(data, "app.session.toggleNamedFilter") {
            return Self::call(&mut self.on_toggle_name_filter);
        }
        if kb.matches(data, "app.session.togglePath") {
            self.show_path = !self.show_path;
            if let Some(on_toggle_path) = self.on_toggle_path.as_mut() {
                on_toggle_path(self.show_path);
            }
            return;
        }
        if kb.matches(data, "app.session.delete") {
            return self.start_delete_confirmation_for_selected_session();
        }
        if kb.matches(data, "app.session.rename") {
            let selected = self.selected_session_path();
            if let (Some(path), Some(on_rename)) = (selected, self.on_rename_session.as_mut()) {
                on_rename(path);
            }
            return;
        }
        // Ctrl+Backspace deletes only with an empty query; otherwise the input gets it.
        if kb.matches(data, "app.session.deleteNoninvasive") {
            if self.search_input.get_value().is_empty() {
                self.start_delete_confirmation_for_selected_session();
            } else {
                self.pass_to_search(data);
            }
            return;
        }
        self.selection_touched = true;
        let last = self.filtered.len().saturating_sub(1);
        if kb.matches(data, "tui.select.up") {
            self.selected_index = self.selected_index.saturating_sub(1);
        } else if kb.matches(data, "tui.select.down") {
            self.selected_index = (self.selected_index + 1).min(last);
        } else if kb.matches(data, "tui.select.pageUp") {
            self.selected_index = self.selected_index.saturating_sub(self.max_visible);
        } else if kb.matches(data, "tui.select.pageDown") {
            self.selected_index = (self.selected_index + self.max_visible).min(last);
        } else if kb.matches(data, "tui.select.confirm") {
            let selected = self.selected_session_path();
            if let (Some(path), Some(on_select)) = (selected, self.on_select.as_mut()) {
                on_select(path);
            }
        } else if kb.matches(data, "tui.select.cancel") {
            Self::call(&mut self.on_cancel);
        } else {
            self.pass_to_search(data);
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.search_input.focused = focused;
    }
}
