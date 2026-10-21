//! The prompt editor with app keybindings and an optional working status in its top border
//! (upstream `components/custom-editor.ts`). [`EditorHandle`] is a cheap clone the mode keeps to
//! change handlers and text while the TUI holds the component.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use pi_tui::components::editor::{BorderContext, EditorCursor};
use pi_tui::components::{EditorOptions, EditorTheme, StyleFn};
use pi_tui::utils::visible_width;
use pi_tui::{AutocompleteProvider, Component, Editor, MouseResponse, Tui, TuiMouseEvent, get_keybindings};

use super::status_indicator::{SharedIndicator, StatusIndicator};
use crate::interactive::prompt_history::PromptHistoryFile;

/// A handler for an app action. Handlers run while the editor component is borrowed, so they
/// must not render the TUI synchronously.
pub type Action = Rc<dyn Fn()>;

/// Runs an extension shortcut for the input; true when one matched.
pub type ShortcutHandler = Rc<dyn Fn(&str) -> bool>;

#[derive(Default)]
struct Handlers {
    actions: Vec<(String, Action)>,
    on_escape: Option<Action>,
    on_ctrl_d: Option<Action>,
    on_paste_image: Option<Action>,
    /// Extension shortcuts, checked before everything else; true when one ran.
    on_extension_shortcut: Option<ShortcutHandler>,
}

/// Shared state of one custom editor.
#[derive(Clone)]
pub struct EditorHandle {
    editor: Editor,
    handlers: Rc<RefCell<Handlers>>,
    working: Rc<RefCell<Option<SharedIndicator>>>,
    embed_working_status: bool,
    history_file: Rc<RefCell<Option<PromptHistoryFile>>>,
    /// Whether the text was typed with a leading space when submit was last pressed.
    leading_space: Rc<Cell<bool>>,
}

impl EditorHandle {
    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    pub fn get_text(&self) -> String {
        self.editor.get_text()
    }

    pub fn get_expanded_text(&self) -> String {
        self.editor.get_expanded_text()
    }

    pub fn set_text(&self, text: &str) {
        self.editor.set_text(text);
    }

    pub fn get_cursor(&self) -> EditorCursor {
        self.editor.get_cursor()
    }

    pub fn insert_text_at_cursor(&self, text: &str) {
        self.editor.insert_text_at_cursor(text);
    }

    pub fn add_to_history(&self, text: &str) {
        self.editor.add_to_history(text);
    }

    /// Loads the saved prompts and appends prompts submitted from now on to `file`.
    pub fn set_prompt_history(&self, file: PromptHistoryFile) {
        self.editor.set_history_limit(file.max_entries().max(100));
        for entry in file.load() {
            self.editor.add_to_history(&entry);
        }
        *self.history_file.borrow_mut() = Some(file);
    }

    /// Adds a submitted prompt to the history and the history file. Input typed with a leading
    /// space stays out of the file, like the shell's `ignorespace`.
    pub fn record_prompt(&self, text: &str) {
        self.editor.add_to_history(text);
        let ignored = self.leading_space.replace(false);
        if let Some(file) = self.history_file.borrow().as_ref().filter(|_| !ignored) {
            // History is best effort: a failed write must not interrupt the prompt.
            let _ = file.append(text);
        }
    }

    pub fn border_color(&self) -> StyleFn {
        self.editor.border_color()
    }

    pub fn set_border_color(&self, style: StyleFn) {
        self.editor.set_border_color(style);
    }

    pub fn set_autocomplete_provider(&self, provider: Rc<dyn AutocompleteProvider>) {
        self.editor.set_autocomplete_provider(provider);
    }

    pub fn embed_working_status(&self) -> bool {
        self.embed_working_status
    }

    /// Register a handler for an `app.*` keybinding id.
    pub fn on_action(&self, action: &str, handler: impl Fn() + 'static) {
        let mut handlers = self.handlers.borrow_mut();
        handlers.actions.retain(|(id, _)| id != action);
        handlers.actions.push((action.to_string(), Rc::new(handler)));
    }

    /// Replace the Escape handler and return the replaced one (upstream swaps `onEscape`).
    pub fn set_on_escape(&self, handler: Option<Action>) -> Option<Action> {
        std::mem::replace(&mut self.handlers.borrow_mut().on_escape, handler)
    }

    pub fn set_on_ctrl_d(&self, handler: Option<Action>) {
        self.handlers.borrow_mut().on_ctrl_d = handler;
    }

    pub fn set_on_paste_image(&self, handler: Option<Action>) {
        self.handlers.borrow_mut().on_paste_image = handler;
    }

    pub fn set_on_extension_shortcut(&self, handler: Option<ShortcutHandler>) {
        self.handlers.borrow_mut().on_extension_shortcut = handler;
    }

    /// The `app.*` actions with handlers, which a custom editor forwards here.
    pub fn action_ids(&self) -> Vec<String> {
        self.handlers.borrow().actions.iter().map(|(id, _)| id.clone()).collect()
    }

    /// Run the handler of `action` as if its key was pressed here.
    pub fn trigger_action(&self, action: &str) {
        if let Some(handler) = self.action(action) {
            handler();
        }
    }

    /// Escape as this editor handles it without autocomplete.
    pub fn trigger_escape(&self) {
        let handler = self.handlers.borrow().on_escape.clone().or_else(|| self.action("app.interrupt"));
        if let Some(handler) = handler {
            handler();
        }
    }

    /// Ctrl+D on an empty editor.
    pub fn trigger_ctrl_d(&self) {
        let handler = self.handlers.borrow().on_ctrl_d.clone().or_else(|| self.action("app.exit"));
        if let Some(handler) = handler {
            handler();
        }
    }

    pub fn trigger_paste_image(&self) {
        let handler = self.handlers.borrow().on_paste_image.clone();
        if let Some(handler) = handler {
            handler();
        }
    }

    pub fn on_submit(&self, callback: impl FnMut(&str) + 'static) {
        self.editor.on_submit(callback);
    }

    pub fn on_change(&self, callback: impl FnMut(&str) + 'static) {
        self.editor.on_change(callback);
    }

    pub fn set_working_status_indicator(&self, indicator: Option<SharedIndicator>) {
        *self.working.borrow_mut() = indicator;
    }

    fn action(&self, id: &str) -> Option<Action> {
        self.handlers.borrow().actions.iter().find(|(action, _)| action == id).map(|(_, handler)| handler.clone())
    }

    /// Upstream `CustomEditor.handleInput`.
    pub fn handle_input(&self, data: &str) {
        // The history search takes every key until it closes.
        if self.editor.is_searching_history() {
            self.editor.handle_input(data);
            return;
        }
        let shortcut = self.handlers.borrow().on_extension_shortcut.clone();
        if shortcut.is_some_and(|shortcut| shortcut(data)) {
            return;
        }
        let keys = get_keybindings();
        let submits = keys.matches(data, "tui.input.submit") || keys.matches(data, "app.message.followUp");
        self.leading_space.set(submits && self.editor.get_text().starts_with(' '));
        if keys.matches(data, "app.clipboard.pasteImage") {
            let handler = self.handlers.borrow().on_paste_image.clone();
            if let Some(handler) = handler {
                handler();
            }
            return;
        }
        if keys.matches(data, "app.interrupt") {
            if !self.editor.is_showing_autocomplete() {
                let handler = self.handlers.borrow().on_escape.clone().or_else(|| self.action("app.interrupt"));
                if let Some(handler) = handler {
                    handler();
                    return;
                }
            }
            self.editor.handle_input(data);
            return;
        }
        if keys.matches(data, "app.exit") && self.editor.get_text().is_empty() {
            let handler = self.handlers.borrow().on_ctrl_d.clone().or_else(|| self.action("app.exit"));
            if let Some(handler) = handler {
                handler();
            }
            return;
        }
        if keys.matches(data, "tui.editor.historyPrevious") || keys.matches(data, "tui.editor.historyNext") {
            self.editor.handle_input(data);
            return;
        }
        let actions = self.handlers.borrow().actions.clone();
        for (action, handler) in actions {
            if action != "app.interrupt" && action != "app.exit" && keys.matches(data, &action) {
                handler();
                return;
            }
        }
        self.editor.handle_input(data);
    }
}

/// The editor component mounted in the editor slot.
pub struct CustomEditor {
    handle: EditorHandle,
}

impl CustomEditor {
    pub fn new(tui: &Tui, theme: EditorTheme, options: EditorOptions, embed_working_status: bool) -> Self {
        let editor = Editor::new(tui, theme, options);
        let working: Rc<RefCell<Option<SharedIndicator>>> = Rc::default();
        if embed_working_status {
            let slot = working.clone();
            editor.set_top_border_renderer(Some(Box::new(move |context: &BorderContext<'_>| {
                let indicator = slot.borrow().clone();
                match indicator.as_ref().map(|indicator| indicator.try_borrow_mut()) {
                    Some(Ok(mut indicator)) => render_status_border(&mut indicator, context),
                    _ => context.default_border(),
                }
            })));
        }
        let handle = EditorHandle {
            editor,
            handlers: Rc::default(),
            working,
            embed_working_status,
            history_file: Rc::default(),
            leading_space: Rc::default(),
        };
        Self { handle }
    }

    pub fn handle(&self) -> EditorHandle {
        self.handle.clone()
    }
}

/// Upstream `renderTopBorder` with an embedded status: `── <status> ────` and, when lines are
/// hidden above, a centered `↑ N more` label if both fit.
fn render_status_border(indicator: &mut StatusIndicator, context: &BorderContext<'_>) -> String {
    let width = context.width;
    if width == 0 {
        return context.default_border();
    }
    let color = context.border_color;
    let mut status = indicator.render_in_border(width.saturating_sub(5).max(1));
    let mut status_width = visible_width(&status);
    if status_width == 0 {
        return context.default_border();
    }
    let hidden = context.hidden_line_count;
    let overflow_label = (hidden > 0).then(|| format!(" ↑ {hidden} more "));
    let overflow_width = overflow_label.as_deref().map_or(0, visible_width);
    let overflow_start = (width as i64 - overflow_width as i64).div_euclid(2);
    let can_fit = |status_width: usize| {
        overflow_label.is_some() && overflow_width + 2 <= width && overflow_start - (3 + status_width as i64 + 1) >= 1
    };
    if overflow_label.is_some() && !can_fit(status_width) {
        status = indicator.render_spinner_in_border(width);
        status_width = visible_width(&status);
    }
    if let Some(label) = overflow_label.as_deref().filter(|_| can_fit(status_width)) {
        let left = 3 + status_width + 1;
        let start = overflow_start as usize;
        let tail = format!(" {}{label}{}", "─".repeat(start - left), "─".repeat(width - start - overflow_width));
        return format!("{}{status}{}", color("── "), color(&tail));
    }
    if width >= status_width + 5 {
        return format!("{}{status}{}", color("── "), color(&format!(" {}", "─".repeat(width - status_width - 4))));
    }
    let status = indicator.render_spinner_in_border(width);
    let status_width = visible_width(&status);
    let prefix = 3.min(width.saturating_sub(status_width));
    let rest = "─".repeat(width.saturating_sub(prefix + status_width));
    format!("{}{status}{}", color(&"─".repeat(prefix)), color(&rest))
}

impl Component for CustomEditor {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.handle.editor.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        self.handle.handle_input(data);
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        self.handle.editor.handle_mouse(event).map(MouseResponse::Handled)
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.handle.editor.set_focused(focused);
    }
}
