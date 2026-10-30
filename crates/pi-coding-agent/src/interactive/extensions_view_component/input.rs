//! Key handling for [`ExtensionsBrowser`]. Ctrl keys are reserved so plain typing goes to search.

use pi_tui::keys::matches_key;
use pi_tui::{Component, get_keybindings};

use super::{BrowserAction, BrowserTab, ExtensionsBrowser, Mode, RowAction};

const PAGE: usize = 10;

pub(super) fn handle(browser: &mut ExtensionsBrowser, data: &str) {
    match browser.mode.clone() {
        Mode::ConfirmRemove { id, name } => handle_confirm(browser, data, id, name),
        Mode::Details { id, action } => handle_details(browser, data, id, action),
        Mode::List => handle_list(browser, data),
    }
}

fn handle_confirm(browser: &mut ExtensionsBrowser, data: &str, id: String, name: String) {
    if data == "y" || data == "Y" {
        if !browser.busy {
            browser.mode = Mode::List;
            browser.emit_mutation(BrowserAction::Remove(id), format!("Removing {name}…"));
        }
        return;
    }
    if data == "n" || data == "N" || get_keybindings().matches(data, "tui.select.cancel") {
        browser.mode = Mode::List;
    }
}

fn handle_details(browser: &mut ExtensionsBrowser, data: &str, id: String, action: usize) {
    let kb = get_keybindings();
    let Some(row) = browser.row_by_id(&id).cloned() else {
        browser.mode = Mode::List;
        return;
    };
    let actions = browser.row_actions(&row);
    if kb.matches(data, "tui.select.cancel") {
        browser.mode = Mode::List;
        return;
    }
    if matches_key(data, "tab") || matches_key(data, "shift+tab") {
        browser.mode = Mode::List;
        handle_list(browser, data);
        return;
    }
    if kb.matches(data, "tui.select.up") && !actions.is_empty() {
        let action = if action == 0 { actions.len() - 1 } else { action - 1 };
        browser.mode = Mode::Details { id, action };
        return;
    }
    if kb.matches(data, "tui.select.down") && !actions.is_empty() {
        let action = if action + 1 >= actions.len() { 0 } else { action + 1 };
        browser.mode = Mode::Details { id, action };
        return;
    }
    if kb.matches(data, "tui.select.confirm") {
        if let Some(chosen) = actions.get(action).copied() {
            browser.request(chosen, &row);
        }
        return;
    }
    handle_shortcut(browser, data);
}

fn handle_list(browser: &mut ExtensionsBrowser, data: &str) {
    let kb = get_keybindings();
    let len = browser.visible().len();
    if kb.matches(data, "tui.select.cancel") {
        if !browser.search.get_value().is_empty() {
            browser.search.set_value("");
            browser.selected = 0;
        } else if !browser.busy {
            browser.emit(BrowserAction::Close);
        }
        return;
    }
    if matches_key(data, "tab") || matches_key(data, "shift+tab") {
        browser.tab = match browser.tab {
            BrowserTab::Installed => BrowserTab::Marketplace,
            BrowserTab::Marketplace => BrowserTab::Installed,
        };
        browser.selected = 0;
        return;
    }
    if kb.matches(data, "tui.select.up") {
        if len > 0 {
            browser.selected = if browser.selected == 0 { len - 1 } else { browser.selected - 1 };
        }
        return;
    }
    if kb.matches(data, "tui.select.down") {
        if len > 0 {
            browser.selected = if browser.selected + 1 >= len { 0 } else { browser.selected + 1 };
        }
        return;
    }
    if kb.matches(data, "tui.select.pageUp") {
        browser.selected = browser.selected.saturating_sub(PAGE);
        return;
    }
    if kb.matches(data, "tui.select.pageDown") {
        browser.selected = (browser.selected + PAGE).min(len.saturating_sub(1));
        return;
    }
    if kb.matches(data, "tui.select.confirm") {
        if let Some(row) = browser.selected_row() {
            browser.mode = Mode::Details { id: row.id.clone(), action: 0 };
        }
        return;
    }
    if space_toggles(browser) && matches_key(data, "space") {
        if let Some(row) = browser.selected_row().cloned()
            && let Some(action) = browser.row_actions(&row).into_iter().find(|action| is_toggle(*action))
        {
            browser.request(action, &row);
        }
        return;
    }
    if handle_shortcut(browser, data) {
        return;
    }
    browser.search.handle_input(data);
    browser.selected = 0;
}

/// Space toggles on the Installed tab while the search box is empty; otherwise it types.
pub(super) fn space_toggles(browser: &ExtensionsBrowser) -> bool {
    browser.tab == BrowserTab::Installed && browser.search.get_value().is_empty()
}

fn is_toggle(action: RowAction) -> bool {
    matches!(action, RowAction::Enable | RowAction::Disable)
}

/// Ctrl+I install, Ctrl+U update, Ctrl+D remove, Ctrl+R reload. Returns true when `data` is one.
/// Legacy terminals send Ctrl+I as Tab, so callers check Tab first.
fn handle_shortcut(browser: &mut ExtensionsBrowser, data: &str) -> bool {
    let wanted = if matches_key(data, "ctrl+i") {
        RowAction::Install
    } else if matches_key(data, "ctrl+u") {
        RowAction::Update
    } else if matches_key(data, "ctrl+d") {
        RowAction::Remove
    } else if matches_key(data, "ctrl+r") {
        if browser.reload_offered && !browser.busy {
            browser.reload_offered = false;
            browser.emit(BrowserAction::Reload);
        }
        return true;
    } else {
        return false;
    };
    let row = match &browser.mode {
        Mode::Details { id, .. } => browser.row_by_id(id).cloned(),
        _ => browser.selected_row().cloned(),
    };
    if let Some(row) = row
        && browser.row_actions(&row).contains(&wanted)
    {
        browser.request(wanted, &row);
    }
    true
}
