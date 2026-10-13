//! Selectors and dialogs of interactive mode (upstream `modes/interactive/components/*`). Each
//! takes data and callbacks and implements [`pi_tui::Component`]. Callbacks run while the TUI
//! borrows the component, so hosts must not borrow it again from inside a callback.

#[allow(dead_code)]
mod common;
pub mod countdown_timer;
pub mod dynamic_border;
pub mod keybinding_hints;
pub mod show_images_selector;
pub mod theme_selector;
pub mod thinking_selector;

pub use countdown_timer::CountdownTimer;
pub use dynamic_border::DynamicBorder;
pub use keybinding_hints::{format_key_text, key_display_text, key_hint, key_text, raw_key_hint};
pub use show_images_selector::ShowImagesSelector;
pub use theme_selector::ThemeSelector;
pub use thinking_selector::ThinkingSelector;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;
