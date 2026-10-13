//! Keybinding hint text (upstream `keybinding-hints.ts`): the keys bound to an id, formatted for
//! display, with the dim key and muted description styling used in dialog footers.

use pi_tui::get_keybindings;

use crate::keybindings::current_platform;
use crate::theme::{ThemeColor, theme};

fn format_key_part(part: &str, capitalize: bool) -> String {
    let part = if current_platform() == "darwin" && part.eq_ignore_ascii_case("alt") { "option" } else { part };
    if !capitalize {
        return part.to_string();
    }
    let mut chars = part.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

fn format_key_text_with(key: &str, capitalize: bool) -> String {
    key.split('/')
        .map(|key| key.split('+').map(|part| format_key_part(part, capitalize)).collect::<Vec<_>>().join("+"))
        .collect::<Vec<_>>()
        .join("/")
}

/// Formats a key like `ctrl+x/alt+y` for display (`alt` shows as `option` on macOS).
pub fn format_key_text(key: &str) -> String {
    format_key_text_with(key, false)
}

/// [`format_key_text`] with capitalized parts, such as `Ctrl+X`.
pub fn format_key_display_text(key: &str) -> String {
    format_key_text_with(key, true)
}

fn format_keys(keys: &[String], capitalize: bool) -> String {
    if keys.is_empty() { String::new() } else { format_key_text_with(&keys.join("/"), capitalize) }
}

/// The keys bound to a keybinding id, such as `ctrl+s/alt+s`.
pub fn key_text(keybinding: &str) -> String {
    format_keys(&get_keybindings().get_keys(keybinding), false)
}

/// The keys bound to a keybinding id with capitalized parts, such as `Ctrl+S`.
pub fn key_display_text(keybinding: &str) -> String {
    format_keys(&get_keybindings().get_keys(keybinding), true)
}

/// The dim key text of `keybinding` followed by a muted description.
pub fn key_hint(keybinding: &str, description: &str) -> String {
    let theme = theme();
    theme.fg(ThemeColor::Dim, &key_text(keybinding)) + &theme.fg(ThemeColor::Muted, &format!(" {description}"))
}

/// Like [`key_hint`] for a literal key such as `↑↓`.
pub fn raw_key_hint(key: &str, description: &str) -> String {
    let theme = theme();
    theme.fg(ThemeColor::Dim, &format_key_text(key)) + &theme.fg(ThemeColor::Muted, &format!(" {description}"))
}
