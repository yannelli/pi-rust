//! Key help rows of the tree selector (upstream `TreeHelp`), wrapped by whole items.

use std::sync::LazyLock;

use pi_tui::Component;
use pi_tui::get_keybindings;
use pi_tui::utils::{visible_width, wrap_text_with_ansi};
use regex::Regex;

use crate::interactive_components::format_key_text;
use crate::theme::{ThemeColor, theme};

struct HelpItem {
    keys: &'static [&'static str],
    label: &'static str,
    label_first: bool,
}

const fn item(keys: &'static [&'static str], label: &'static str, label_first: bool) -> HelpItem {
    HelpItem { keys, label, label_first }
}

const TREE_HELP_ITEMS: &[HelpItem] = &[
    item(&["tui.select.up", "tui.select.down"], "move", false),
    item(&["tui.editor.cursorLeft", "tui.editor.cursorRight"], "page", false),
    item(&["app.tree.foldOrUp", "app.tree.unfoldOrDown"], "branch", false),
    item(&["app.message.copy"], "copy", false),
    item(&["app.tree.editLabel"], "label", false),
    item(&["app.tree.toggleLabelTimestamp"], "label time", false),
    item(
        &[
            "app.tree.filter.default",
            "app.tree.filter.noTools",
            "app.tree.filter.userOnly",
            "app.tree.filter.labeledOnly",
            "app.tree.filter.all",
        ],
        "filters",
        true,
    ),
    item(&["app.tree.filter.cycleForward", "app.tree.filter.cycleBackward"], "cycle", true),
];

static KEY_NAMES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [("pageUp", "pgup"), ("pageDown", "pgdn"), ("up", "↑"), ("down", "↓"), ("left", "←"), ("right", "→")]
        .into_iter()
        .map(|(name, short)| (Regex::new(&format!(r"(?-u:\b){name}(?-u:\b)")).expect("key name regex"), short))
        .collect()
});

/// Shares a common modifier prefix: `ctrl+d/ctrl+t` becomes `ctrl+d/t`.
fn compact_raw_keys(keys: &[String]) -> String {
    if keys.len() == 1 {
        return keys[0].clone();
    }
    let parts: Vec<(&str, &str)> = keys
        .iter()
        .map(|key| match key.rfind('+') {
            Some(index) => (&key[..=index], &key[index + 1..]),
            None => ("", key.as_str()),
        })
        .collect();
    let prefix = parts[0].0;
    if !prefix.is_empty() && parts.iter().all(|(part, _)| *part == prefix) {
        let suffixes: Vec<&str> = parts.iter().map(|(_, suffix)| *suffix).collect();
        format!("{prefix}{}", suffixes.join("/"))
    } else {
        keys.join("/")
    }
}

/// The first key of each binding, compacted, with arrow and page keys as symbols.
fn format_help_keys(keybindings: &[&str]) -> String {
    let manager = get_keybindings();
    let keys: Vec<String> =
        keybindings.iter().filter_map(|binding| manager.get_keys(binding).into_iter().next()).collect();
    if keys.is_empty() {
        return String::new();
    }
    KEY_NAMES.iter().fold(format_key_text(&compact_raw_keys(&keys)), |text, (pattern, short)| {
        pattern.replace_all(&text, *short).into_owned()
    })
}

/// Help rows: items joined with ` · `, wrapped between items, indented when they fit.
#[derive(Default)]
pub(super) struct TreeHelp;

impl Component for TreeHelp {
    fn render(&mut self, width: usize) -> Vec<String> {
        let items = TREE_HELP_ITEMS.iter().map(|HelpItem { keys, label, label_first }| {
            let text = format_help_keys(keys);
            if text.is_empty() {
                label.to_string()
            } else if *label_first {
                format!("{label} {text}")
            } else {
                format!("{text} {label}")
            }
        });
        let available = width.max(1);
        let start = |item: &str| {
            let indented = format!("  {item}");
            if visible_width(&indented) <= available { indented } else { item.to_string() }
        };
        let mut lines = Vec::new();
        let mut current = String::new();
        for item in items {
            if current.is_empty() {
                current = start(&item);
                continue;
            }
            let candidate = format!("{current} · {item}");
            if visible_width(&candidate) <= available {
                current = candidate;
                continue;
            }
            lines.extend(wrap_text_with_ansi(current.trim_end(), available));
            current = start(&item);
        }
        if !current.is_empty() {
            lines.extend(wrap_text_with_ansi(current.trim_end(), available));
        }
        let theme = theme();
        lines.iter().map(|line| theme.fg(ThemeColor::Muted, line)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compacts_keys_with_a_shared_modifier() {
        let keys = |list: &[&str]| list.iter().map(|key| key.to_string()).collect::<Vec<_>>();
        assert_eq!(compact_raw_keys(&keys(&["ctrl+d", "ctrl+t", "ctrl+u"])), "ctrl+d/t/u");
        assert_eq!(compact_raw_keys(&keys(&["ctrl+o", "shift+ctrl+o"])), "ctrl+o/shift+ctrl+o");
        assert_eq!(compact_raw_keys(&keys(&["up", "down"])), "up/down");
        assert_eq!(compact_raw_keys(&keys(&["shift+l"])), "shift+l");
    }
}
