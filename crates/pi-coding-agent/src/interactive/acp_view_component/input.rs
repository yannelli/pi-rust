//! Key handling for [`AcpBrowser`]. Plain typing goes to the search box.

use pi_tui::keys::matches_key;
use pi_tui::{Component, get_keybindings};

use super::{AcpAction, AcpBrowser, AcpTab, Mode};

const PAGE: usize = 10;

pub(super) fn handle(browser: &mut AcpBrowser, data: &str) {
    match browser.mode.clone() {
        Mode::ConfirmRemove { id, name } => handle_confirm(browser, data, id, name),
        Mode::Menu { id, item } => handle_menu(browser, data, id, item),
        Mode::List => handle_list(browser, data),
    }
}

fn handle_confirm(browser: &mut AcpBrowser, data: &str, id: String, name: String) {
    if data == "y" || data == "Y" {
        if !browser.busy {
            browser.mode = Mode::List;
            browser.emit_mutation(AcpAction::Remove(id), format!("Removing {name}…"));
        }
        return;
    }
    if data == "n" || data == "N" || get_keybindings().matches(data, "tui.select.cancel") {
        browser.mode = Mode::List;
    }
}

fn handle_menu(browser: &mut AcpBrowser, data: &str, id: String, item: usize) {
    let kb = get_keybindings();
    let Some(row) = browser.installed_row(&id).cloned() else {
        browser.mode = Mode::List;
        return;
    };
    let items = AcpBrowser::menu_items(&row);
    if kb.matches(data, "tui.select.cancel") {
        browser.mode = Mode::List;
    } else if kb.matches(data, "tui.select.up") {
        let item = if item == 0 { items.len() - 1 } else { item - 1 };
        browser.mode = Mode::Menu { id, item };
    } else if kb.matches(data, "tui.select.down") {
        let item = if item + 1 >= items.len() { 0 } else { item + 1 };
        browser.mode = Mode::Menu { id, item };
    } else if kb.matches(data, "tui.select.confirm")
        && let Some(chosen) = items.get(item).copied()
    {
        browser.run(chosen, &row);
    }
}

fn handle_list(browser: &mut AcpBrowser, data: &str) {
    let kb = get_keybindings();
    let len = browser.visible().len();
    if kb.matches(data, "tui.select.cancel") {
        if !browser.search.get_value().is_empty() {
            browser.search.set_value("");
            browser.selected = 0;
        } else if !browser.busy {
            browser.emit(AcpAction::Close);
        }
    } else if matches_key(data, "tab") || matches_key(data, "shift+tab") {
        browser.tab = match browser.tab {
            AcpTab::Installed => AcpTab::Registry,
            AcpTab::Registry => AcpTab::Installed,
        };
        browser.selected = 0;
    } else if kb.matches(data, "tui.select.up") {
        if len > 0 {
            browser.selected = if browser.selected == 0 { len - 1 } else { browser.selected - 1 };
        }
    } else if kb.matches(data, "tui.select.down") {
        if len > 0 {
            browser.selected = if browser.selected + 1 >= len { 0 } else { browser.selected + 1 };
        }
    } else if kb.matches(data, "tui.select.pageUp") {
        browser.selected = browser.selected.saturating_sub(PAGE);
    } else if kb.matches(data, "tui.select.pageDown") {
        browser.selected = (browser.selected + PAGE).min(len.saturating_sub(1));
    } else if kb.matches(data, "tui.select.confirm") {
        let Some(row) = browser.selected_row().cloned() else { return };
        match browser.tab {
            AcpTab::Installed => browser.mode = Mode::Menu { id: row.id, item: 0 },
            AcpTab::Registry => browser.install(&row),
        }
    } else {
        browser.search.handle_input(data);
        browser.selected = 0;
    }
}
