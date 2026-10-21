//! Keybinding definitions (default keys and descriptions) and the built-in TUI set.

/// Default keys and description for one keybinding id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingDefinition {
    /// Keys bound when the user has no override. Empty means unbound by default.
    pub default_keys: Vec<String>,
    /// Text shown in help and hotkey listings.
    pub description: Option<String>,
}

impl KeybindingDefinition {
    /// Creates a definition from key ids and a description.
    pub fn new(default_keys: &[&str], description: &str) -> Self {
        Self {
            default_keys: default_keys.iter().map(|key| (*key).to_string()).collect(),
            description: Some(description.to_string()),
        }
    }
}

/// Keybinding definitions by id, in declaration order. Callers extend the TUI set with their
/// own ids; inserting an existing id replaces it in place, like a JavaScript object spread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeybindingDefinitions {
    entries: Vec<(String, KeybindingDefinition)>,
}

impl KeybindingDefinitions {
    /// Creates an empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces the definition for `id` and returns the old one.
    pub fn insert(&mut self, id: impl Into<String>, definition: KeybindingDefinition) -> Option<KeybindingDefinition> {
        let id = id.into();
        match self.entries.iter_mut().find(|(existing, _)| *existing == id) {
            Some((_, slot)) => Some(std::mem::replace(slot, definition)),
            None => {
                self.entries.push((id, definition));
                None
            }
        }
    }

    /// The definition for `id`, if any.
    pub fn get(&self, id: &str) -> Option<&KeybindingDefinition> {
        self.entries.iter().find(|(existing, _)| existing == id).map(|(_, definition)| definition)
    }

    /// Mutable access to the definition for `id`, if any.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut KeybindingDefinition> {
        self.entries.iter_mut().find(|(existing, _)| existing == id).map(|(_, definition)| definition)
    }

    /// Returns true when `id` is defined.
    pub fn contains(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    /// Number of definitions.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true when there are no definitions.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates over `(id, definition)` pairs in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &KeybindingDefinition)> {
        self.entries.iter().map(|(id, definition)| (id.as_str(), definition))
    }

    /// Iterates over the ids in declaration order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(id, _)| id.as_str())
    }
}

impl<K: Into<String>> FromIterator<(K, KeybindingDefinition)> for KeybindingDefinitions {
    fn from_iter<I: IntoIterator<Item = (K, KeybindingDefinition)>>(iter: I) -> Self {
        let mut definitions = Self::new();
        definitions.extend(iter);
        definitions
    }
}

impl<K: Into<String>> Extend<(K, KeybindingDefinition)> for KeybindingDefinitions {
    fn extend<I: IntoIterator<Item = (K, KeybindingDefinition)>>(&mut self, iter: I) {
        for (id, definition) in iter {
            self.insert(id, definition);
        }
    }
}

impl IntoIterator for KeybindingDefinitions {
    type Item = (String, KeybindingDefinition);
    type IntoIter = std::vec::IntoIter<(String, KeybindingDefinition)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

macro_rules! tui_bindings {
    ($($name:ident = $id:literal, [$($key:literal),*], $description:literal;)*) => {
        /// Ids of the built-in TUI keybindings.
        pub mod ids {
            $(#[doc = concat!("`", $id, "`: ", $description, ".")] pub const $name: &str = $id;)*
            /// Every built-in TUI keybinding id, in declaration order.
            pub const ALL: &[&str] = &[$($id),*];
        }

        /// The built-in TUI keybindings (upstream `TUI_KEYBINDINGS`), in upstream order, plus the
        /// pi-rust `tui.editor.historySearch`.
        pub fn tui_keybindings() -> KeybindingDefinitions {
            [$(($id, KeybindingDefinition::new(&[$($key),*], $description))),*].into_iter().collect()
        }
    };
}

tui_bindings! {
    EDITOR_CURSOR_UP = "tui.editor.cursorUp", ["up"], "Move cursor up";
    EDITOR_CURSOR_DOWN = "tui.editor.cursorDown", ["down"], "Move cursor down";
    EDITOR_HISTORY_PREVIOUS = "tui.editor.historyPrevious", [], "Select previous prompt history entry";
    EDITOR_HISTORY_NEXT = "tui.editor.historyNext", [], "Select next prompt history entry";
    EDITOR_HISTORY_SEARCH = "tui.editor.historySearch", ["ctrl+r"], "Search prompt history";
    EDITOR_CURSOR_LEFT = "tui.editor.cursorLeft", ["left", "ctrl+b"], "Move cursor left";
    EDITOR_CURSOR_RIGHT = "tui.editor.cursorRight", ["right", "ctrl+f"], "Move cursor right";
    EDITOR_CURSOR_WORD_LEFT = "tui.editor.cursorWordLeft", ["alt+left", "ctrl+left", "alt+b"], "Move cursor word left";
    EDITOR_CURSOR_WORD_RIGHT = "tui.editor.cursorWordRight", ["alt+right", "ctrl+right", "alt+f"],
        "Move cursor word right";
    EDITOR_CURSOR_LINE_START = "tui.editor.cursorLineStart", ["home", "ctrl+a"], "Move to line start";
    EDITOR_CURSOR_LINE_END = "tui.editor.cursorLineEnd", ["end", "ctrl+e"], "Move to line end";
    EDITOR_JUMP_FORWARD = "tui.editor.jumpForward", ["ctrl+]"], "Jump forward to character";
    EDITOR_JUMP_BACKWARD = "tui.editor.jumpBackward", ["ctrl+alt+]"], "Jump backward to character";
    EDITOR_PAGE_UP = "tui.editor.pageUp", ["pageUp", "ctrl+pageUp"], "Page up";
    EDITOR_PAGE_DOWN = "tui.editor.pageDown", ["pageDown", "ctrl+pageDown"], "Page down";
    EDITOR_DELETE_CHAR_BACKWARD = "tui.editor.deleteCharBackward", ["backspace"], "Delete character backward";
    EDITOR_DELETE_CHAR_FORWARD = "tui.editor.deleteCharForward", ["delete", "ctrl+d"], "Delete character forward";
    EDITOR_DELETE_WORD_BACKWARD = "tui.editor.deleteWordBackward", ["ctrl+w", "alt+backspace"],
        "Delete word backward";
    EDITOR_DELETE_WORD_FORWARD = "tui.editor.deleteWordForward", ["alt+d", "alt+delete"], "Delete word forward";
    EDITOR_DELETE_TO_LINE_START = "tui.editor.deleteToLineStart", ["ctrl+u"], "Delete to line start";
    EDITOR_DELETE_TO_LINE_END = "tui.editor.deleteToLineEnd", ["ctrl+k"], "Delete to line end";
    EDITOR_YANK = "tui.editor.yank", ["ctrl+y"], "Yank";
    EDITOR_YANK_POP = "tui.editor.yankPop", ["alt+y"], "Yank pop";
    EDITOR_UNDO = "tui.editor.undo", ["ctrl+-"], "Undo";
    INPUT_NEW_LINE = "tui.input.newLine", ["shift+enter", "ctrl+j"], "Insert newline";
    INPUT_SUBMIT = "tui.input.submit", ["enter"], "Submit input";
    INPUT_TAB = "tui.input.tab", ["tab"], "Tab / autocomplete";
    INPUT_COPY = "tui.input.copy", ["ctrl+c"], "Copy selection";
    SELECT_UP = "tui.select.up", ["up"], "Move selection up";
    SELECT_DOWN = "tui.select.down", ["down"], "Move selection down";
    SELECT_PAGE_UP = "tui.select.pageUp", ["pageUp"], "Selection page up";
    SELECT_PAGE_DOWN = "tui.select.pageDown", ["pageDown"], "Selection page down";
    SELECT_CONFIRM = "tui.select.confirm", ["enter"], "Confirm selection";
    SELECT_CANCEL = "tui.select.cancel", ["escape", "ctrl+c"], "Cancel selection";
    ALT_SCREEN_PAGE_UP = "tui.altScreen.pageUp", ["pageUp"], "Scroll viewport up one page";
    ALT_SCREEN_PAGE_DOWN = "tui.altScreen.pageDown", ["pageDown"], "Scroll viewport down one page";
    ALT_SCREEN_HALF_PAGE_UP = "tui.altScreen.halfPageUp", [], "Scroll viewport up half a page";
    ALT_SCREEN_HALF_PAGE_DOWN = "tui.altScreen.halfPageDown", [], "Scroll viewport down half a page";
    ALT_SCREEN_LINE_UP = "tui.altScreen.lineUp", [], "Scroll viewport up one line";
    ALT_SCREEN_LINE_DOWN = "tui.altScreen.lineDown", [], "Scroll viewport down one line";
    ALT_SCREEN_PREVIOUS_PROMPT = "tui.altScreen.previousPrompt", ["ctrl+shift+up", "ctrl+up"],
        "Jump to previous semantic prompt";
    ALT_SCREEN_NEXT_PROMPT = "tui.altScreen.nextPrompt", ["ctrl+shift+down", "ctrl+down"],
        "Jump to next semantic prompt";
    ALT_SCREEN_SEARCH = "tui.altScreen.search", ["ctrl+shift+f"], "Search the primary scroll view";
    ALT_SCREEN_SEARCH_NEXT = "tui.altScreen.searchNext", ["enter", "ctrl+g"], "Select the next search match";
    ALT_SCREEN_SEARCH_PREVIOUS = "tui.altScreen.searchPrevious", ["shift+enter", "ctrl+shift+g"],
        "Select the previous search match";
    ALT_SCREEN_SEARCH_CLOSE = "tui.altScreen.searchClose", ["escape"], "Close transcript search";
    ALT_SCREEN_TOP = "tui.altScreen.top", ["ctrl+home"], "Scroll viewport to top";
    ALT_SCREEN_BOTTOM = "tui.altScreen.bottom", ["ctrl+end"], "Scroll viewport to bottom";
}
