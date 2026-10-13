//! Multi-line prompt editor (upstream `components/editor.ts`): history, kill ring, undo, paste
//! markers, word wrap, scrolling, autocomplete, and mouse positioning. Cursor columns are UTF-8
//! byte offsets; [`Editor`] is a cheap-clone handle to one editor.

mod autocomplete;
mod editing;
mod input;
mod movement;
mod popup;
mod render;
mod segment;
mod state;
mod task;
mod wrap;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub use render::{BorderContext, BorderRenderer, scroll_border};
pub use segment::TextSegment;
pub use wrap::{TextChunk, word_wrap_line, word_wrap_line_with_segments};

use crate::autocomplete::AutocompleteProvider;
use crate::component::{Component, MouseResponse, TuiMouseEvent, TuiMouseEventResult};
use crate::components::scroll_view::StyleFn;
use crate::editor_component::{EditorComponent, EditorTextCallback};
use crate::tui::Tui;
use state::EditorCore;

/// Styles for the autocomplete popup.
pub use crate::components::select_list::SelectListTheme;

/// Editor styles.
#[derive(Clone)]
pub struct EditorTheme {
    pub border_color: StyleFn,
    pub select_list: SelectListTheme,
}

/// Construction options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorOptions {
    /// Horizontal padding inside the borders.
    pub padding_x: usize,
    /// Autocomplete rows, clamped to 3..=20.
    pub autocomplete_max_visible: usize,
}

impl Default for EditorOptions {
    fn default() -> Self {
        Self { padding_x: 0, autocomplete_max_visible: 5 }
    }
}

/// Cursor position: logical line and byte offset in it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditorCursor {
    pub line: usize,
    pub col: usize,
}

/// Notifications queued while the editor state is borrowed and delivered afterwards.
enum EditorEvent {
    Change(String),
    Submit(String),
}

#[derive(Default)]
struct Callbacks {
    on_change: Option<EditorTextCallback>,
    on_submit: Option<EditorTextCallback>,
    change_generation: u64,
    submit_generation: u64,
}

struct Shared {
    core: RefCell<EditorCore>,
    callbacks: RefCell<Callbacks>,
    dispatching: Cell<bool>,
}

struct DispatchGuard<'a>(&'a Cell<bool>);

impl Drop for DispatchGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl Shared {
    /// Delivers queued change and submit events. Callbacks run with no editor borrow held, so they
    /// may call back into the editor through another handle.
    fn dispatch(&self) {
        if self.dispatching.replace(true) {
            return;
        }
        let _guard = DispatchGuard(&self.dispatching);
        loop {
            let Some(event) = self.core.borrow_mut().events.pop_front() else { break };
            let (is_change, text) = match event {
                EditorEvent::Change(text) => (true, text),
                EditorEvent::Submit(text) => (false, text),
            };
            let (callback, generation) = {
                let mut callbacks = self.callbacks.borrow_mut();
                if is_change {
                    (callbacks.on_change.take(), callbacks.change_generation)
                } else {
                    (callbacks.on_submit.take(), callbacks.submit_generation)
                }
            };
            let Some(mut callback) = callback else { continue };
            callback(&text);
            let mut callbacks = self.callbacks.borrow_mut();
            let callbacks = &mut *callbacks;
            let (slot, current) = if is_change {
                (&mut callbacks.on_change, callbacks.change_generation)
            } else {
                (&mut callbacks.on_submit, callbacks.submit_generation)
            };
            if current == generation && slot.is_none() {
                *slot = Some(callback);
            }
        }
    }
}

/// The multi-line editor. Clones share one editor, so callbacks can hold a clone. Callbacks run
/// after the editor finishes the operation that triggered them.
#[derive(Clone)]
pub struct Editor {
    shared: Rc<Shared>,
}

impl Editor {
    pub fn new(tui: &Tui, theme: EditorTheme, options: EditorOptions) -> Self {
        let shared = Rc::new_cyclic(|weak| Shared {
            core: RefCell::new(EditorCore::new(tui.downgrade(), theme, &options, weak.clone())),
            callbacks: RefCell::new(Callbacks::default()),
            dispatching: Cell::new(false),
        });
        Self { shared }
    }

    /// Runs `f` on the state, then delivers queued events.
    fn with<R>(&self, f: impl FnOnce(&mut EditorCore) -> R) -> R {
        let result = f(&mut self.shared.core.borrow_mut());
        self.shared.dispatch();
        result
    }

    fn read<R>(&self, f: impl FnOnce(&EditorCore) -> R) -> R {
        f(&self.shared.core.borrow())
    }

    /// Feeds terminal input (keys, bracketed paste).
    pub fn handle_input(&self, data: &str) {
        self.with(|core| core.handle_input(data));
    }

    /// Renders the editor for `width` columns.
    pub fn render(&self, width: usize) -> Vec<String> {
        self.with(|core| core.render(width))
    }

    /// Mouse handling: a click places the cursor; the popup takes clicks and wheel.
    pub fn handle_mouse(&self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        self.with(|core| core.handle_mouse(event))
    }

    /// The text, lines joined with `\n`.
    pub fn get_text(&self) -> String {
        self.read(EditorCore::get_text)
    }

    /// The text with paste markers replaced by the pasted content.
    pub fn get_expanded_text(&self) -> String {
        self.read(EditorCore::get_expanded_text)
    }

    /// A copy of the logical lines.
    pub fn get_lines(&self) -> Vec<String> {
        self.read(|core| core.state.lines.clone())
    }

    pub fn get_cursor(&self) -> EditorCursor {
        self.read(|core| EditorCursor { line: core.state.cursor_line, col: core.state.cursor_col })
    }

    /// Replaces the text (undoable) and clears paste markers.
    pub fn set_text(&self, text: &str) {
        self.with(|core| core.set_text(text));
    }

    /// Inserts text at the cursor as one undo step.
    pub fn insert_text_at_cursor(&self, text: &str) {
        self.with(|core| core.insert_text_at_cursor(text));
    }

    /// Adds a submitted prompt to the history for Up and Down.
    pub fn add_to_history(&self, text: &str) {
        self.with(|core| core.add_to_history(text));
    }

    pub fn set_on_submit(&self, callback: Option<EditorTextCallback>) {
        let mut callbacks = self.shared.callbacks.borrow_mut();
        callbacks.on_submit = callback;
        callbacks.submit_generation += 1;
    }

    pub fn set_on_change(&self, callback: Option<EditorTextCallback>) {
        let mut callbacks = self.shared.callbacks.borrow_mut();
        callbacks.on_change = callback;
        callbacks.change_generation += 1;
    }

    /// Sets the submit callback.
    pub fn on_submit(&self, callback: impl FnMut(&str) + 'static) {
        self.set_on_submit(Some(Box::new(callback)));
    }

    /// Sets the change callback.
    pub fn on_change(&self, callback: impl FnMut(&str) + 'static) {
        self.set_on_change(Some(Box::new(callback)));
    }

    /// While set, Enter does nothing (upstream `disableSubmit`).
    pub fn set_disable_submit(&self, disabled: bool) {
        self.with(|core| core.disable_submit = disabled);
    }

    pub fn disable_submit(&self) -> bool {
        self.read(|core| core.disable_submit)
    }

    pub fn get_padding_x(&self) -> usize {
        self.read(|core| core.padding_x)
    }

    pub fn set_padding_x(&self, padding: usize) {
        self.with(|core| core.set_padding_x(padding));
    }

    pub fn get_autocomplete_max_visible(&self) -> usize {
        self.read(|core| core.autocomplete_max_visible)
    }

    pub fn set_autocomplete_max_visible(&self, max_visible: usize) {
        self.with(|core| core.set_autocomplete_max_visible(max_visible));
    }

    /// Replaces the autocomplete provider and its trigger characters.
    pub fn set_autocomplete_provider(&self, provider: Rc<dyn AutocompleteProvider>) {
        self.with(|core| core.set_autocomplete_provider(provider));
    }

    pub fn is_showing_autocomplete(&self) -> bool {
        self.read(|core| core.autocomplete_mode.is_some())
    }

    pub fn border_color(&self) -> StyleFn {
        self.read(|core| core.border_color.clone())
    }

    pub fn set_border_color(&self, style: StyleFn) {
        self.with(|core| core.border_color = style);
    }

    /// Replaces the top border (upstream subclasses override `renderTopBorder`).
    pub fn set_top_border_renderer(&self, renderer: Option<BorderRenderer>) {
        self.with(|core| core.top_border = renderer);
    }

    /// Replaces the bottom border.
    pub fn set_bottom_border_renderer(&self, renderer: Option<BorderRenderer>) {
        self.with(|core| core.bottom_border = renderer);
    }

    pub fn is_focused(&self) -> bool {
        self.read(|core| core.focused)
    }

    pub fn set_focused(&self, focused: bool) {
        self.with(|core| core.focused = focused);
    }
}

impl Component for Editor {
    fn render(&mut self, width: usize) -> Vec<String> {
        Editor::render(self, width)
    }

    fn handle_input(&mut self, data: &str) {
        Editor::handle_input(self, data);
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        Editor::handle_mouse(self, event).map(MouseResponse::Handled)
    }

    fn invalidate(&mut self) {}

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        Editor::set_focused(self, focused);
    }
}

impl EditorComponent for Editor {
    fn get_text(&self) -> String {
        Editor::get_text(self)
    }

    fn set_text(&mut self, text: &str) {
        Editor::set_text(self, text);
    }

    fn set_on_submit(&mut self, callback: Option<EditorTextCallback>) {
        Editor::set_on_submit(self, callback);
    }

    fn set_on_change(&mut self, callback: Option<EditorTextCallback>) {
        Editor::set_on_change(self, callback);
    }

    fn add_to_history(&mut self, text: &str) {
        Editor::add_to_history(self, text);
    }

    fn insert_text_at_cursor(&mut self, text: &str) {
        Editor::insert_text_at_cursor(self, text);
    }

    fn get_expanded_text(&self) -> String {
        Editor::get_expanded_text(self)
    }

    fn border_color(&self) -> Option<StyleFn> {
        Some(Editor::border_color(self))
    }

    fn set_border_color(&mut self, style: StyleFn) {
        Editor::set_border_color(self, style);
    }

    fn supports_padding_x(&self) -> bool {
        true
    }

    fn set_padding_x(&mut self, padding: usize) {
        Editor::set_padding_x(self, padding);
    }

    fn supports_autocomplete_max_visible(&self) -> bool {
        true
    }

    fn set_autocomplete_max_visible(&mut self, max_visible: usize) {
        Editor::set_autocomplete_max_visible(self, max_visible);
    }

    fn set_autocomplete_provider(&mut self, provider: Rc<dyn AutocompleteProvider>) {
        Editor::set_autocomplete_provider(self, provider);
    }
}
