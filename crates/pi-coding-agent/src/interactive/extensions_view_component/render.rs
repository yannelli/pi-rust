//! Rendering for [`ExtensionsBrowser`]: tabs, search, rows or details, status, and key hints.

use pi_tui::Component;
use pi_tui::utils::{truncate_to_width, wrap_text_with_ansi};

use super::input::space_toggles;
use super::{BrowserTab, ExtensionRow, ExtensionsBrowser, Mode};
use crate::interactive_components::{key_hint, raw_key_hint};
use crate::theme::{ThemeColor, theme};

const MAX_VISIBLE: usize = 10;

pub(super) fn render(browser: &mut ExtensionsBrowser, width: usize) -> Vec<String> {
    let t = theme();
    let mut lines = vec![t.fg(ThemeColor::Border, &"─".repeat(width))];
    lines.push(format!(" {}", t.fg(ThemeColor::Accent, &t.bold("Extensions"))));
    lines.push(tabs_line(browser));
    lines.extend(browser.search.render(width));
    lines.push(String::new());
    match browser.mode.clone() {
        Mode::List => lines.extend(list_lines(browser, width)),
        Mode::Details { id, action } => lines.extend(details_lines(browser, &id, action, width)),
        Mode::ConfirmRemove { id, name } => {
            if browser.row_by_id(&id).is_some() {
                lines.extend(details_lines(browser, &id, usize::MAX, width));
            }
            let prompt = format!(" Remove {name}? y confirm · n cancel");
            lines.push(t.fg(ThemeColor::Warning, &prompt));
        }
    }
    lines.push(String::new());
    if browser.reload_offered {
        lines.push(t.fg(ThemeColor::Warning, " Extensions changed. Ctrl+R to reload."));
    }
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

fn tabs_line(browser: &ExtensionsBrowser) -> String {
    let t = theme();
    let catalog_count = if browser.catalog_loaded { format!(" ({})", browser.catalog.len()) } else { String::new() };
    let tab = |label: String, active: bool| {
        if active { t.fg(ThemeColor::Accent, &t.bold(&format!("[{label}]"))) } else { t.fg(ThemeColor::Muted, &label) }
    };
    format!(
        " {}  {}",
        tab(format!("Installed ({})", browser.installed.len()), browser.tab == BrowserTab::Installed),
        tab(format!("Marketplace{catalog_count}"), browser.tab == BrowserTab::Marketplace),
    )
}

fn list_lines(browser: &ExtensionsBrowser, width: usize) -> Vec<String> {
    let t = theme();
    if browser.tab == BrowserTab::Marketplace {
        if let Some(error) = &browser.catalog_error {
            let mut lines =
                wrap_text_with_ansi(&t.fg(ThemeColor::Warning, &format!(" Marketplace unavailable: {error}")), width);
            lines.push(t.fg(ThemeColor::Muted, " Installed extensions still work. Tab to return."));
            if browser.catalog.is_empty() {
                return lines;
            }
            lines.push(String::new());
            lines.extend(row_lines(browser, width));
            return lines;
        }
        if !browser.catalog_loaded {
            return vec![t.fg(ThemeColor::Muted, " Loading marketplace…")];
        }
    }
    if browser.rows().is_empty() {
        let empty = match browser.tab {
            BrowserTab::Installed => " No extensions installed",
            BrowserTab::Marketplace => " No marketplace extensions",
        };
        return vec![t.fg(ThemeColor::Muted, empty)];
    }
    row_lines(browser, width)
}

fn row_lines(browser: &ExtensionsBrowser, width: usize) -> Vec<String> {
    let t = theme();
    let visible = browser.visible();
    if visible.is_empty() {
        return vec![t.fg(ThemeColor::Muted, " No matching extensions")];
    }
    let selected = browser.selected.min(visible.len() - 1);
    let start = selected.saturating_sub(MAX_VISIBLE / 2).min(visible.len().saturating_sub(MAX_VISIBLE));
    let end = (start + MAX_VISIBLE).min(visible.len());
    let mut lines: Vec<String> = (start..end)
        .map(|position| row_line(&browser.rows()[visible[position]], position == selected, width))
        .collect();
    if start > 0 || end < visible.len() {
        lines.push(t.fg(ThemeColor::Muted, &format!("  ({}/{})", selected + 1, visible.len())));
    }
    lines
}

fn row_line(row: &ExtensionRow, selected: bool, width: usize) -> String {
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

fn markers(row: &ExtensionRow) -> String {
    let t = theme();
    let mut out = String::new();
    if !row.version.is_empty() {
        out.push_str(&t.fg(ThemeColor::Muted, &format!(" v{}", row.version.trim_start_matches('v'))));
    }
    if !row.source.is_empty() {
        out.push_str(&t.fg(ThemeColor::Dim, &format!(" [{}]", row.source)));
    }
    if row.enabled == Some(false) {
        out.push_str(&t.fg(ThemeColor::Muted, " disabled"));
    }
    if row.update_available {
        out.push_str(&t.fg(ThemeColor::Warning, " update available"));
    }
    out
}

fn details_lines(browser: &ExtensionsBrowser, id: &str, action: usize, width: usize) -> Vec<String> {
    let t = theme();
    let Some(row) = browser.row_by_id(id) else {
        return Vec::new();
    };
    let mut lines = vec![format!(" {}{}", t.fg(ThemeColor::Accent, &t.bold(&row.name)), markers(row))];
    if !row.description.is_empty() {
        lines.extend(wrap_text_with_ansi(&format!(" {}", row.description), width));
    }
    let mut field = |label: &str, value: &str| {
        if !value.is_empty() {
            lines.push(format!(" {} {value}", t.fg(ThemeColor::Muted, &format!("{label}:"))));
        }
    };
    field("ID", &row.id);
    field("Source", &row.source);
    field("Path", &row.path);
    field("Tags", &row.tags.join(", "));
    let state = match (row.installed, row.enabled) {
        (_, Some(true)) => "installed, enabled",
        (_, Some(false)) => "installed, disabled",
        (true, None) => "installed",
        (false, None) => "not installed",
    };
    field("Status", state);
    for detail in &row.details {
        lines.extend(wrap_text_with_ansi(&t.fg(ThemeColor::Dim, &format!(" {detail}")), width));
    }
    let actions = browser.row_actions(row);
    if action != usize::MAX && !actions.is_empty() {
        lines.push(String::new());
        lines.push(t.fg(ThemeColor::Muted, " Actions"));
        for (index, item) in actions.iter().enumerate() {
            let label = if browser.busy { t.fg(ThemeColor::Dim, item.label()) } else { item.label().to_string() };
            let line = if index == action {
                format!(" {}{}", t.fg(ThemeColor::Accent, "→ "), t.fg(ThemeColor::Accent, &label))
            } else {
                format!("   {label}")
            };
            lines.push(line);
        }
    }
    lines
}

fn hints(browser: &ExtensionsBrowser) -> Vec<String> {
    let t = theme();
    let mut hints = Vec::new();
    if browser.busy {
        hints.push(t.fg(ThemeColor::Warning, "busy: changes disabled"));
    }
    match browser.mode {
        Mode::ConfirmRemove { .. } => return hints,
        Mode::Details { .. } => {
            hints.push(raw_key_hint("↑↓", "choose"));
            hints.push(key_hint("tui.select.confirm", "run"));
            hints.push(key_hint("tui.select.cancel", "back"));
            return hints;
        }
        Mode::List => {}
    }
    hints.push(raw_key_hint("↑↓", "select"));
    hints.push(key_hint("tui.select.confirm", "details and actions"));
    match browser.tab {
        BrowserTab::Installed => {
            if space_toggles(browser) {
                hints.push(raw_key_hint("space", "toggle"));
            }
            hints.push(raw_key_hint("ctrl+u", "update"));
            hints.push(raw_key_hint("ctrl+d", "remove"));
            hints.push(raw_key_hint("tab", "marketplace"));
        }
        BrowserTab::Marketplace => {
            hints.push(raw_key_hint("ctrl+i", "install (Kitty keyboard)"));
            hints.push(raw_key_hint("ctrl+u", "update"));
            hints.push(raw_key_hint("ctrl+d", "remove"));
            hints.push(raw_key_hint("tab", "installed"));
        }
    }
    if browser.reload_offered {
        hints.push(raw_key_hint("ctrl+r", "reload"));
    }
    let close = if browser.search.get_value().is_empty() { "close" } else { "clear search" };
    hints.push(key_hint("tui.select.cancel", close));
    hints
}
