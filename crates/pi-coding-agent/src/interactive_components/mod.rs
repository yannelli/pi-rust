//! Selectors and dialogs of interactive mode (upstream `modes/interactive/components/*`). Each
//! takes data and callbacks and implements [`pi_tui::Component`]. Callbacks run while the TUI
//! borrows the component, so hosts must not borrow it again from inside a callback.

pub mod armin;
mod common;
pub mod config_selector;
pub mod countdown_timer;
pub mod dynamic_border;
pub mod earendil_announcement;
pub mod easter_egg_3d;
pub mod extension_editor;
pub mod extension_input;
pub mod extension_selector;
pub mod external_editor;
pub mod first_time_setup;
pub mod keybinding_hints;
pub mod login;
pub mod model_selector;
pub mod scoped_models_selector;
pub mod session_selector;
pub mod settings_selector;
pub mod show_images_selector;
pub mod theme_selector;
pub mod thinking_selector;
pub mod tree_selector;
pub mod trust_selector;
pub mod user_message_selector;

pub use armin::ArminComponent;
pub use config_selector::{ConfigSelector, config_selector_fn, run_config_selector};
pub use countdown_timer::CountdownTimer;
pub use dynamic_border::DynamicBorder;
pub use earendil_announcement::EarendilAnnouncementComponent;
pub use easter_egg_3d::{EasterEgg3d, play_armin_3d, play_easter_egg_3d, play_pi_logo_3d};
pub use extension_editor::{ExtensionEditor, ExtensionEditorOptions};
pub use extension_input::{ExtensionInput, ExtensionInputOptions};
pub use extension_selector::{ExtensionSelector, ExtensionSelectorOptions};
pub use external_editor::{ExternalEditorResult, edit_in_external_editor};
pub use first_time_setup::{FirstTimeSetupComponent, FirstTimeSetupResult, apply_first_time_setup_result};
pub use keybinding_hints::{
    format_key_display_text, format_key_text, key_display_text, key_hint, key_text, raw_key_hint,
};
pub use model_selector::{ModelSelector, ModelSelectorOptions, ModelSelectorSource};
pub use scoped_models_selector::{EnabledIds, ModelsCallbacks, ModelsConfig, ScopedModelsSelector};
pub use session_selector::{SessionSelector, SessionSelectorOptions};
pub use settings_selector::{SettingsCallbacks, SettingsConfig, SettingsSelector};
pub use show_images_selector::ShowImagesSelector;
pub use theme_selector::ThemeSelector;
pub use thinking_selector::ThinkingSelector;
pub use tree_selector::{BranchSummaryFlow, BranchSummaryStep, FilterMode, TreeData, TreeSelector};
pub use trust_selector::{TrustSelection, TrustSelector, TrustSelectorOptions};
pub use user_message_selector::{UserMessageItem, UserMessageSelector};

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;
