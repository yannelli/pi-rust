//! Selectors and dialogs of interactive mode (upstream `modes/interactive/components/*`). Each
//! takes data and callbacks and implements [`pi_tui::Component`]. Callbacks run while the TUI
//! borrows the component, so hosts must not borrow it again from inside a callback.

pub mod armin;
#[allow(dead_code)]
mod common;
pub mod config_selector;
pub mod countdown_timer;
pub mod dynamic_border;
pub mod earendil_announcement;
pub mod easter_egg_3d;
pub mod first_time_setup;
pub mod keybinding_hints;
pub mod session_selector;
pub mod settings_selector;
pub mod show_images_selector;
pub mod theme_selector;
pub mod thinking_selector;
pub mod tree_selector;
pub mod user_message_selector;

pub use countdown_timer::CountdownTimer;
pub use dynamic_border::DynamicBorder;
pub use keybinding_hints::{
    format_key_display_text, format_key_text, key_display_text, key_hint, key_text, raw_key_hint,
};
pub use show_images_selector::ShowImagesSelector;
pub use theme_selector::ThemeSelector;
pub use thinking_selector::ThinkingSelector;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;
