//! Editor state, construction, text access, and prompt history.

use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use regex::{NoExpand, Regex};

use super::popup::AutocompleteList;
use super::render::BorderRenderer;
use super::segment::{PasteRegistry, TextSegment, grapheme_segments};
use super::{EditorEvent, EditorOptions, EditorTheme, Shared};
use crate::autocomplete::{AbortController, AutocompleteProvider};
use crate::components::scroll_view::StyleFn;
use crate::kill_ring::KillRing;
use crate::tui::WeakTui;
use crate::ui_loop::TimerId;
use crate::undo_stack::UndoStack;

const MAX_HISTORY: usize = 100;
pub(super) const DEFAULT_TRIGGER_CHARACTERS: [&str; 2] = ["@", "#"];

/// Text and cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TextState {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
}

impl Default for TextState {
    fn default() -> Self {
        Self { lines: vec![String::new()], cursor_line: 0, cursor_col: 0 }
    }
}

/// Undo snapshot: text state plus the paste registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EditorSnapshot {
    pub state: TextState,
    pub pastes: PasteRegistry,
    pub paste_counter: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum JumpDirection {
    Forward,
    Backward,
}

/// Whether the popup came from typing or from an explicit Tab (force) request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AutocompleteMode {
    Regular,
    Force,
}

/// The editor's mutable state. All editing logic is implemented on this type.
pub(crate) struct EditorCore {
    pub state: TextState,
    pub focused: bool,
    pub tui: WeakTui,
    pub theme: EditorTheme,
    pub padding_x: usize,
    pub border_color: StyleFn,
    pub top_border: Option<BorderRenderer>,
    pub bottom_border: Option<BorderRenderer>,
    pub disable_submit: bool,
    pub events: VecDeque<EditorEvent>,
    /// Handle used by timers and autocomplete tasks to reach this state later.
    pub self_ref: Weak<Shared>,

    // Render geometry for cursor movement and mouse hit testing.
    pub last_width: usize,
    pub rendered_visible_line_count: usize,
    pub rendered_autocomplete_height: usize,
    pub scroll_offset: usize,

    pub provider: Option<Rc<dyn AutocompleteProvider>>,
    pub trigger_characters: Vec<String>,
    pub autocomplete_list: Option<AutocompleteList>,
    pub autocomplete_mode: Option<AutocompleteMode>,
    pub autocomplete_prefix: String,
    pub autocomplete_max_visible: usize,
    pub autocomplete_abort: Option<AbortController>,
    pub autocomplete_debounce: Option<TimerId>,
    pub autocomplete_latch: Option<super::task::Latch>,
    pub autocomplete_start_token: u64,
    pub autocomplete_request_id: u64,

    pub pastes: PasteRegistry,
    pub paste_counter: u32,
    pub paste_buffer: String,
    pub is_in_paste: bool,

    pub history: VecDeque<String>,
    /// `None` when not browsing; 0 is the most recent entry.
    pub history_index: Option<usize>,
    pub history_draft: Option<TextState>,

    pub kill_ring: KillRing,
    pub last_action: Option<LastAction>,
    pub jump_mode: Option<JumpDirection>,
    /// Sticky column for vertical movement.
    pub preferred_visual_col: Option<usize>,
    /// The column before the cursor snapped to the start of a paste marker.
    pub snapped_from_cursor_col: Option<usize>,
    pub undo_stack: UndoStack<EditorSnapshot>,
}

pub(super) fn clamp_max_visible(max_visible: usize) -> usize {
    max_visible.clamp(3, 20)
}

impl EditorCore {
    pub fn new(tui: WeakTui, theme: EditorTheme, options: &EditorOptions, self_ref: Weak<Shared>) -> Self {
        Self {
            state: TextState::default(),
            focused: false,
            tui,
            border_color: theme.border_color.clone(),
            theme,
            padding_x: options.padding_x,
            top_border: None,
            bottom_border: None,
            disable_submit: false,
            events: VecDeque::new(),
            self_ref,
            last_width: 80,
            rendered_visible_line_count: 1,
            rendered_autocomplete_height: 0,
            scroll_offset: 0,
            provider: None,
            trigger_characters: DEFAULT_TRIGGER_CHARACTERS.iter().map(|c| c.to_string()).collect(),
            autocomplete_list: None,
            autocomplete_mode: None,
            autocomplete_prefix: String::new(),
            autocomplete_max_visible: clamp_max_visible(options.autocomplete_max_visible),
            autocomplete_abort: None,
            autocomplete_debounce: None,
            autocomplete_latch: None,
            autocomplete_start_token: 0,
            autocomplete_request_id: 0,
            pastes: PasteRegistry::default(),
            paste_counter: 0,
            paste_buffer: String::new(),
            is_in_paste: false,
            history: VecDeque::new(),
            history_index: None,
            history_draft: None,
            kill_ring: KillRing::new(),
            last_action: None,
            jump_mode: None,
            preferred_visual_col: None,
            snapped_from_cursor_col: None,
            undo_stack: UndoStack::new(),
        }
    }

    /// Asks the TUI for a render, if it still exists.
    pub fn request_render(&self) {
        if let Some(tui) = self.tui.upgrade() {
            tui.request_render(false);
        }
    }

    /// Terminal height, for the visible line limit and paging.
    pub fn terminal_rows(&self) -> usize {
        self.tui
            .upgrade()
            .and_then(|tui| tui.terminal().try_borrow().ok().map(|terminal| terminal.rows()))
            .unwrap_or(24)
    }

    pub fn emit_change(&mut self) {
        let text = self.get_text();
        self.events.push_back(EditorEvent::Change(text));
    }

    pub fn current_line(&self) -> &str {
        self.state.lines.get(self.state.cursor_line).map_or("", String::as_str)
    }

    pub fn text_before_cursor(&self) -> &str {
        let line = self.current_line();
        &line[..self.state.cursor_col.min(line.len())]
    }

    /// Graphemes with registered paste markers merged.
    pub fn graphemes<'a>(&self, text: &'a str) -> Vec<TextSegment<'a>> {
        grapheme_segments(text, &self.pastes)
    }

    pub fn get_text(&self) -> String {
        self.state.lines.join("\n")
    }

    fn expand_paste_markers(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (id, content) in self.pastes.iter() {
            let pattern = format!(r"\[paste #{id}( (\+\d+ lines|\d+ chars))?\]");
            if let Ok(regex) = Regex::new(&pattern) {
                result = regex.replace_all(&result, NoExpand(content)).into_owned();
            }
        }
        result
    }

    pub fn get_expanded_text(&self) -> String {
        self.expand_paste_markers(&self.get_text())
    }

    /// Text submitted on Enter: markers expanded, trimmed.
    pub fn submission_text(&self) -> String {
        crate::utils::js_trim(&self.get_expanded_text()).to_string()
    }

    pub fn set_padding_x(&mut self, padding: usize) {
        if self.padding_x != padding {
            self.padding_x = padding;
            self.request_render();
        }
    }

    pub fn set_autocomplete_max_visible(&mut self, max_visible: usize) {
        let max_visible = clamp_max_visible(max_visible);
        if self.autocomplete_max_visible != max_visible {
            self.autocomplete_max_visible = max_visible;
            self.request_render();
        }
    }

    /// Line endings become `\n`; tabs become four spaces.
    pub fn normalize_text(text: &str) -> String {
        text.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ")
    }

    pub fn set_text(&mut self, text: &str) {
        self.cancel_autocomplete();
        self.last_action = None;
        self.exit_history_browsing();
        let normalized = Self::normalize_text(text);
        // Programmatic changes are undoable.
        if self.get_text() != normalized {
            self.push_undo_snapshot();
        }
        self.pastes.clear();
        self.paste_counter = 0;
        self.set_text_internal(&normalized, true);
    }

    /// Replaces the text without touching history state. The cursor goes to the end, or to the
    /// start when `at_end` is false.
    pub fn set_text_internal(&mut self, text: &str, at_end: bool) {
        self.state.lines = text.split('\n').map(str::to_string).collect();
        self.state.cursor_line = if at_end { self.state.lines.len() - 1 } else { 0 };
        let col = if at_end { self.current_line().len() } else { 0 };
        self.set_cursor_col(col);
        // Render adjusts the scroll to show the cursor.
        self.scroll_offset = 0;
        self.emit_change();
    }

    /// Sets the column and clears the sticky column, for every non-vertical movement.
    pub fn set_cursor_col(&mut self, col: usize) {
        self.state.cursor_col = col;
        self.preferred_visual_col = None;
        self.snapped_from_cursor_col = None;
    }

    pub fn add_to_history(&mut self, text: &str) {
        let trimmed = crate::utils::js_trim(text);
        if trimmed.is_empty() || self.history.front().is_some_and(|newest| newest == trimmed) {
            return;
        }
        self.history.push_front(trimmed.to_string());
        if self.history.len() > MAX_HISTORY {
            self.history.pop_back();
        }
    }

    pub fn is_editor_empty(&self) -> bool {
        self.state.lines.len() == 1 && self.state.lines[0].is_empty()
    }

    /// Up (`-1`) moves to older entries, Down (`1`) to newer ones and finally back to the draft.
    pub fn navigate_history(&mut self, direction: isize) {
        self.last_action = None;
        if self.history.is_empty() {
            return;
        }
        let current = self.history_index.map_or(-1, |index| index as isize);
        let new_index = current - direction;
        if new_index < -1 || new_index >= self.history.len() as isize {
            return;
        }
        if self.history_index.is_none() && new_index >= 0 {
            self.push_undo_snapshot();
            self.history_draft = Some(self.state.clone());
        }
        if new_index < 0 {
            self.history_index = None;
            match self.history_draft.take() {
                Some(draft) => {
                    self.state = draft;
                    self.preferred_visual_col = None;
                    self.snapped_from_cursor_col = None;
                    self.scroll_offset = 0;
                    self.emit_change();
                }
                None => self.set_text_internal("", true),
            }
        } else {
            let index = new_index as usize;
            self.history_index = Some(index);
            let entry = self.history[index].clone();
            self.set_text_internal(&entry, direction != -1);
        }
    }

    pub fn exit_history_browsing(&mut self) {
        self.history_index = None;
        self.history_draft = None;
    }

    pub fn push_undo_snapshot(&mut self) {
        self.undo_stack.push(&EditorSnapshot {
            state: self.state.clone(),
            pastes: self.pastes.clone(),
            paste_counter: self.paste_counter,
        });
    }

    pub fn undo(&mut self) {
        self.exit_history_browsing();
        let Some(snapshot) = self.undo_stack.pop() else { return };
        self.state = snapshot.state;
        self.pastes = snapshot.pastes;
        self.paste_counter = snapshot.paste_counter;
        self.last_action = None;
        self.preferred_visual_col = None;
        self.emit_change();
    }
}
