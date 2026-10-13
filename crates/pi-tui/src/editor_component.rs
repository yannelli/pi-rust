//! Interface for editor components, so extensions can replace the default editor
//! (upstream `editor-component.ts`).

use crate::component::Component;
use crate::components::scroll_view::StyleFn;

/// Callback receiving the editor text.
pub type EditorTextCallback = Box<dyn FnMut(&str)>;

/// A custom editor (vim mode, emacs mode, ...). Methods with defaults are optional upstream and
/// do nothing unless overridden. The autocomplete provider setter lives with the autocomplete
/// module, which extends this interface.
pub trait EditorComponent: Component {
    /// Current text.
    fn get_text(&self) -> String;

    /// Replaces the text.
    fn set_text(&mut self, text: &str);

    /// Called when the user submits (for example with Enter).
    fn set_on_submit(&mut self, callback: Option<EditorTextCallback>);

    /// Called when the text changes.
    fn set_on_change(&mut self, callback: Option<EditorTextCallback>);

    /// Adds text to the history for up and down navigation.
    fn add_to_history(&mut self, _text: &str) {}

    /// Inserts text at the cursor.
    fn insert_text_at_cursor(&mut self, _text: &str) {}

    /// Text with markers (such as paste markers) expanded. Defaults to [`EditorComponent::get_text`].
    fn get_expanded_text(&self) -> String {
        self.get_text()
    }

    /// Border color style, if the editor draws a border.
    fn border_color(&self) -> Option<StyleFn> {
        None
    }

    fn set_border_color(&mut self, _style: StyleFn) {}

    /// Whether [`EditorComponent::set_padding_x`] is supported.
    fn supports_padding_x(&self) -> bool {
        false
    }

    /// Horizontal padding.
    fn set_padding_x(&mut self, _padding: usize) {}

    /// Whether [`EditorComponent::set_autocomplete_max_visible`] is supported.
    fn supports_autocomplete_max_visible(&self) -> bool {
        false
    }

    /// Maximum visible items in the autocomplete dropdown.
    fn set_autocomplete_max_visible(&mut self, _max_visible: usize) {}
}
