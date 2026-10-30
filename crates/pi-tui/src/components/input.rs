//! Single-line text input with horizontal scrolling, bracketed paste, an Emacs kill ring, and undo.
//!
//! Cursor positions are UTF-8 byte offsets on char boundaries (upstream uses UTF-16 indices).

mod editing;
mod render;

use crate::component::{
    Component, MouseResponse, TuiMouseButton, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType,
};
use crate::components::scroll_view::StyleFn;
use crate::keybindings::{get_keybindings, ids};
use crate::keys::decode_kitty_printable;
use crate::kill_ring::KillRing;
use crate::undo_stack::UndoStack;
use crate::utils::{graphemes, visible_width};

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// Snapshot stored on the undo stack.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InputState {
    value: String,
    cursor: usize,
}

/// The last editing action, used for kill accumulation, yank-pop, and undo coalescing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

/// Options for [`Input::new`].
#[derive(Default, Clone)]
pub struct InputOptions {
    /// Text drawn before the value. Defaults to `"> "`.
    pub prompt: Option<String>,
    /// Text shown while the value is empty. Defaults to none.
    pub placeholder: Option<String>,
    /// Styles the placeholder text. Defaults to no styling.
    pub placeholder_style: Option<StyleFn>,
    /// Renders `*` for each grapheme of the value (pi-rust, for secrets).
    pub masked: bool,
}

/// Callback for [`Input::on_submit`]; receives the current value.
pub type InputSubmitCallback = Box<dyn FnMut(&str)>;

/// Callback for [`Input::on_escape`].
pub type InputEscapeCallback = Box<dyn FnMut()>;

/// Single-line text input with horizontal scrolling.
pub struct Input {
    value: String,
    cursor: usize,
    prompt: String,
    placeholder: String,
    placeholder_style: StyleFn,
    masked: bool,
    rendered_start_column: usize,
    /// Called with the value when the submit key (Enter) is pressed.
    pub on_submit: Option<InputSubmitCallback>,
    /// Called when the cancel key (Escape) is pressed.
    pub on_escape: Option<InputEscapeCallback>,
    /// Set by the TUI when focus changes. A focused input emits [`crate::component::CURSOR_MARKER`].
    pub focused: bool,
    paste_buffer: String,
    is_in_paste: bool,
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    undo_stack: UndoStack<InputState>,
}

/// Editing action bound to a keybinding id.
type KeyAction = fn(&mut Input);

/// Key actions checked in upstream order after cancel, undo, and submit.
const KEY_ACTIONS: &[(&str, KeyAction)] = &[
    (ids::EDITOR_DELETE_CHAR_BACKWARD, Input::handle_backspace),
    (ids::EDITOR_DELETE_CHAR_FORWARD, Input::handle_forward_delete),
    (ids::EDITOR_DELETE_WORD_BACKWARD, Input::delete_word_backward),
    (ids::EDITOR_DELETE_WORD_FORWARD, Input::delete_word_forward),
    (ids::EDITOR_DELETE_TO_LINE_START, Input::delete_to_line_start),
    (ids::EDITOR_DELETE_TO_LINE_END, Input::delete_to_line_end),
    (ids::EDITOR_YANK, Input::yank),
    (ids::EDITOR_YANK_POP, Input::yank_pop),
    (ids::EDITOR_CURSOR_LEFT, Input::move_left),
    (ids::EDITOR_CURSOR_RIGHT, Input::move_right),
    (ids::EDITOR_CURSOR_LINE_START, Input::move_line_start),
    (ids::EDITOR_CURSOR_LINE_END, Input::move_line_end),
    (ids::EDITOR_CURSOR_WORD_LEFT, Input::move_word_backward),
    (ids::EDITOR_CURSOR_WORD_RIGHT, Input::move_word_forward),
];

impl Default for Input {
    fn default() -> Self {
        Self::new(InputOptions::default())
    }
}

impl Input {
    /// Creates an empty input.
    pub fn new(options: InputOptions) -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            prompt: options.prompt.unwrap_or_else(|| "> ".to_string()),
            placeholder: options.placeholder.unwrap_or_default(),
            placeholder_style: options.placeholder_style.unwrap_or_else(|| std::rc::Rc::new(str::to_string)),
            masked: options.masked,
            rendered_start_column: 0,
            on_submit: None,
            on_escape: None,
            focused: false,
            paste_buffer: String::new(),
            is_in_paste: false,
            kill_ring: KillRing::new(),
            last_action: None,
            undo_stack: UndoStack::new(),
        }
    }

    /// The current value.
    pub fn get_value(&self) -> &str {
        &self.value
    }

    /// Replaces the value. The cursor stays put, clamped to the new length and a char boundary.
    pub fn set_value(&mut self, value: &str) {
        self.value = value.to_string();
        self.cursor = floor_char_boundary(&self.value, self.cursor);
    }

    /// Sets the submit callback.
    pub fn set_on_submit(&mut self, callback: impl FnMut(&str) + 'static) {
        self.on_submit = Some(Box::new(callback));
    }

    /// Sets the escape callback.
    pub fn set_on_escape(&mut self, callback: impl FnMut() + 'static) {
        self.on_escape = Some(Box::new(callback));
    }

    /// Feeds one input chunk, buffering bracketed paste until its end marker arrives.
    fn process_input(&mut self, data: &str) {
        let stripped: String;
        let mut data = data;
        if data.contains(PASTE_START) {
            self.is_in_paste = true;
            self.paste_buffer.clear();
            stripped = data.replacen(PASTE_START, "", 1);
            data = &stripped;
        }

        if self.is_in_paste {
            self.paste_buffer.push_str(data);
            if let Some(end) = self.paste_buffer.find(PASTE_END) {
                let content = self.paste_buffer[..end].to_string();
                self.handle_paste(&content);
                self.is_in_paste = false;
                let remaining = self.paste_buffer[end + PASTE_END.len()..].to_string();
                self.paste_buffer.clear();
                if !remaining.is_empty() {
                    self.process_input(&remaining);
                }
            }
            return;
        }

        self.handle_key(data);
    }

    fn handle_key(&mut self, data: &str) {
        let kb = get_keybindings();

        if kb.matches(data, ids::SELECT_CANCEL) {
            if let Some(on_escape) = self.on_escape.as_mut() {
                on_escape();
            }
            return;
        }
        if kb.matches(data, ids::EDITOR_UNDO) {
            self.undo();
            return;
        }
        if kb.matches(data, ids::INPUT_SUBMIT) || data == "\n" {
            if let Some(on_submit) = self.on_submit.as_mut() {
                on_submit(&self.value);
            }
            return;
        }
        if let Some((_, action)) = KEY_ACTIONS.iter().find(|(id, _)| kb.matches(data, id)) {
            action(self);
            return;
        }

        // Kitty CSI-u printable keys (for example `\x1b[97u` for `a`) contain ESC, so decode them
        // before the control character check rejects them.
        if let Some(text) = decode_kitty_printable(data) {
            self.insert_character(&text);
            return;
        }
        if !data.chars().any(is_control_char) {
            self.insert_character(data);
        }
    }

    /// Upstream `handleMouse`: a left press on the row moves the cursor to the clicked column.
    fn mouse_press(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        if event.kind != TuiMouseEventType::Press || event.button != TuiMouseButton::Left || event.y != 0 {
            return None;
        }
        // Upstream subtracts the width of the default `"> "` prompt, whatever the prompt is.
        let visible_column = usize::try_from(event.x.saturating_sub(2)).unwrap_or(0);
        let target_column = self.rendered_start_column + visible_column;
        let mut current_column = 0;
        let mut offset = 0;
        self.cursor = self.value.len();
        for grapheme in graphemes(&self.value) {
            let next_column = current_column + visible_width(grapheme);
            if target_column < next_column {
                self.cursor = offset;
                break;
            }
            current_column = next_column;
            offset += grapheme.len();
        }
        self.last_action = None;
        Some(TuiMouseEventResult { handled: true, focus: true, ..TuiMouseEventResult::default() })
    }
}

impl Component for Input {
    fn render(&mut self, width: usize) -> Vec<String> {
        vec![self.render_line(width)]
    }

    fn handle_input(&mut self, data: &str) {
        self.process_input(data);
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        self.mouse_press(event).map(MouseResponse::Handled)
    }

    fn invalidate(&mut self) {}

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

/// C0 controls, DEL, and C1 controls are rejected as typed text.
fn is_control_char(c: char) -> bool {
    let code = u32::from(c);
    code < 0x20 || code == 0x7f || (0x80..=0x9f).contains(&code)
}

/// Largest char boundary of `text` at or before `index`.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}
