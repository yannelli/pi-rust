//! Rendering for [`AcpBrowser`]: tabs, search, rows or the action menu, status, and key hints.

use pi_tui::Component;
use pi_tui::utils::{truncate_to_width, wrap_text_with_ansi};

use super::{AcpBrowser, AcpRow, AcpTab, Mode};
use crate::interactive_components::{key_hint, raw_key_hint};
use crate::theme::{ThemeColor, theme};

const MAX_VISIBLE: usize = 10;

pub(super) fn render(browser: &mut AcpBrowser, width: usize) -> Vec<String> {
    let t = theme();
    let mut lines = vec![t.fg(ThemeColor::Border, &"─".repeat(width))];
    lines.push(format!(" {}", t.fg(ThemeColor::Accent, &t.bold("ACP agents"))));
    lines.push(tabs_line(browser));
    lines.extend(browser.search.render(width));
    lines.push(String::new());
    match browser.mode.clone() {
        Mode::List => lines.extend(list_lines(browser, width)),
        Mode::Menu { id, item } => lines.extend(menu_lines(browser, &id, Some(item), width)),
        Mode::ConfirmRemove { id, name } => {
            lines.extend(menu_lines(browser, &id, None, width));
            lines.push(t.fg(ThemeColor::Warning, &format!(" Remove {name}? y confirm · n cancel")));
        }
    }
    lines.push(String::new());
    if let Some(status) = &browser.status {
        let color = if browser.busy { ThemeColor::Accent } else { ThemeColor::Muted };
        let prefix = if browser.busy { "⋯ " } else { "" };
        lines.extend(wrap_text_with_ansi(&t.fg(color, &format!(" {prefix}{status}")), width));
    }
    lines.extend(wrap_text_with_ansi(
        &format!(" {}", hints(browser).join(t.fg(ThemeColor::Dim, " · ").as_str())),
        width,
    ));
    lines.push(t.fg(ThemeColor::Border, &"─".repeat(width)));
    lines.into_iter().map(|line| truncate_to_width(&line, width)).collect()
}

fn tabs_line(browser: &AcpBrowser) -> String {
    let t = theme();
    let count = if browser.registry_loaded { format!(" ({})", browser.registry.len()) } else { String::new() };
    let tab = |label: String, active: bool| {
        if active { t.fg(ThemeColor::Accent, &t.bold(&format!("[{label}]"))) } else { t.fg(ThemeColor::Muted, &label) }
    };
    format!(
        " {}  {}",
        tab(format!("Installed ({})", browser.installed.len()), browser.tab == AcpTab::Installed),
        tab(format!("Registry{count}"), browser.tab == AcpTab::Registry),
    )
}

fn list_lines(browser: &AcpBrowser, width: usize) -> Vec<String> {
    let t = theme();
    let mut lines = Vec::new();
    if browser.tab == AcpTab::Registry {
        if !browser.registry_loaded {
            return vec![t.fg(ThemeColor::Muted, " Loading the ACP registry…")];
        }
        if let Some(error) = &browser.registry_error {
            let text = format!(" Registry unavailable: {error}. Built-in agents are listed.");
            lines.extend(wrap_text_with_ansi(&t.fg(ThemeColor::Warning, &text), width));
        }
    }
    if browser.rows().is_empty() {
        let empty = match browser.tab {
            AcpTab::Installed => " No ACP agents installed. Tab opens the registry.",
            AcpTab::Registry => " No registry agents",
        };
        lines.push(t.fg(ThemeColor::Muted, empty));
        return lines;
    }
    let visible = browser.visible();
    if visible.is_empty() {
        lines.push(t.fg(ThemeColor::Muted, " No matching agents"));
        return lines;
    }
    let selected = browser.selected.min(visible.len() - 1);
    let start = selected.saturating_sub(MAX_VISIBLE / 2).min(visible.len().saturating_sub(MAX_VISIBLE));
    let end = (start + MAX_VISIBLE).min(visible.len());
    for (position, index) in visible.iter().enumerate().take(end).skip(start) {
        lines.push(row_line(&browser.rows()[*index], position == selected, width));
    }
    if start > 0 || end < visible.len() {
        lines.push(t.fg(ThemeColor::Muted, &format!("  ({}/{})", selected + 1, visible.len())));
    }
    lines
}

fn row_line(row: &AcpRow, selected: bool, width: usize) -> String {
    let t = theme();
    let prefix = if selected { t.fg(ThemeColor::Accent, "→ ") } else { "  ".to_string() };
    let state = match row.enabled {
        Some(true) => t.fg(ThemeColor::Success, "● "),
        Some(false) => t.fg(ThemeColor::Muted, "○ "),
        None if row.installed => t.fg(ThemeColor::Success, "✓ "),
        None => "  ".to_string(),
    };
    let name = if selected { t.fg(ThemeColor::Accent, &row.name) } else { row.name.clone() };
    let mut line = format!(" {prefix}{state}{name}{}", markers(row));
    if !row.description.is_empty() {
        line.push_str(&t.fg(ThemeColor::Dim, &format!("  {}", row.description)));
    }
    truncate_to_width(&line, width)
}

fn markers(row: &AcpRow) -> String {
    let t = theme();
    let mut out = String::new();
    if !row.version.is_empty() {
        out.push_str(&t.fg(ThemeColor::Muted, &format!(" v{}", row.version.trim_start_matches('v'))));
    }
    if !row.scope.is_empty() {
        out.push_str(&t.fg(ThemeColor::Dim, &format!(" [{}]", row.scope)));
    }
    if row.builtin {
        out.push_str(&t.fg(ThemeColor::Dim, " built-in"));
    }
    if row.is_default {
        out.push_str(&t.fg(ThemeColor::Success, " default"));
    }
    if row.enabled == Some(false) {
        out.push_str(&t.fg(ThemeColor::Muted, " disabled"));
    }
    if row.enabled.is_none() && row.installed {
        out.push_str(&t.fg(ThemeColor::Muted, " installed"));
    }
    out
}

/// The installed agent `id` with its details, and its actions when `item` is set.
fn menu_lines(browser: &AcpBrowser, id: &str, item: Option<usize>, width: usize) -> Vec<String> {
    let t = theme();
    let Some(row) = browser.installed_row(id) else { return Vec::new() };
    let mut lines = vec![format!(" {}{}", t.fg(ThemeColor::Accent, &t.bold(&row.name)), markers(row))];
    if !row.description.is_empty() {
        lines.extend(wrap_text_with_ansi(&format!(" {}", row.description), width));
    }
    for detail in &row.details {
        lines.extend(wrap_text_with_ansi(&t.fg(ThemeColor::Dim, &format!(" {detail}")), width));
    }
    let Some(item) = item else { return lines };
    lines.push(String::new());
    lines.push(t.fg(ThemeColor::Muted, " Actions"));
    for (index, entry) in AcpBrowser::menu_items(row).iter().enumerate() {
        let label = if browser.busy { t.fg(ThemeColor::Dim, entry.label()) } else { entry.label().to_string() };
        lines.push(if index == item {
            format!(" {}{}", t.fg(ThemeColor::Accent, "→ "), t.fg(ThemeColor::Accent, &label))
        } else {
            format!("   {label}")
        });
    }
    lines
}

fn hints(browser: &AcpBrowser) -> Vec<String> {
    let t = theme();
    let mut hints = Vec::new();
    if browser.busy {
        hints.push(t.fg(ThemeColor::Warning, "busy: changes disabled"));
    }
    match browser.mode {
        Mode::ConfirmRemove { .. } => return hints,
        Mode::Menu { .. } => {
            hints.push(raw_key_hint("↑↓", "choose"));
            hints.push(key_hint("tui.select.confirm", "run"));
            hints.push(key_hint("tui.select.cancel", "back"));
            return hints;
        }
        Mode::List => {}
    }
    hints.push(raw_key_hint("↑↓", "select"));
    match browser.tab {
        AcpTab::Installed => {
            hints.push(key_hint("tui.select.confirm", "actions"));
            hints.push(raw_key_hint("tab", "registry"));
        }
        AcpTab::Registry => {
            hints.push(key_hint("tui.select.confirm", "install"));
            hints.push(raw_key_hint("tab", "installed"));
        }
    }
    let close = if browser.search.get_value().is_empty() { "close" } else { "clear search" };
    hints.push(key_hint("tui.select.cancel", close));
    hints
}
