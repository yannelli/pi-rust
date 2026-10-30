use pi_tui::Component;

use super::BrowserAction;
use super::tests_render::{browser, row};
use crate::interactive_components::test_support::{keys, render_text, setup};

fn type_text(browser: &mut super::ExtensionsBrowser, text: &str) {
    for ch in text.chars() {
        browser.handle_input(&ch.to_string());
    }
}

#[test]
fn search_filters_rows_and_escape_clears_then_closes() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    type_text(&mut browser, "linter");
    let text = render_text(&mut browser, 120);
    assert!(text.contains("linter"), "{text}");
    assert!(!text.contains("git-helper"), "{text}");
    type_text(&mut browser, "zzz");
    assert!(render_text(&mut browser, 120).contains("No matching extensions"));
    browser.handle_input(keys::ESCAPE);
    assert!(render_text(&mut browser, 120).contains("git-helper"));
    assert!(calls.is_empty());
    browser.handle_input(keys::ESCAPE);
    assert_eq!(calls.take(), vec![BrowserAction::Close]);
}

#[test]
fn space_toggles_when_search_is_empty_and_types_otherwise() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    browser.handle_input(keys::SPACE);
    assert_eq!(calls.take(), vec![BrowserAction::Toggle("package:user:git".to_string())]);
    browser.set_status(String::new(), false);
    type_text(&mut browser, "git x");
    assert!(calls.is_empty());
    assert!(render_text(&mut browser, 120).contains("> git x"));
}

#[test]
fn mutation_sets_busy_before_callback_and_blocks_repeats() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    browser.handle_input(keys::SPACE);
    browser.handle_input(keys::SPACE);
    browser.handle_input(&keys::ctrl('u'));
    assert_eq!(calls.take(), vec![BrowserAction::Toggle("package:user:git".to_string())]);
    assert!(render_text(&mut browser, 120).contains("Disabling git-helper"));
}

#[test]
fn tab_switches_to_marketplace_and_enter_installs() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    browser.set_catalog(vec![row("todo", "todo", None, false), row("git", "git-helper", Some(true), true)], None);
    browser.handle_input(keys::TAB);
    let text = render_text(&mut browser, 120);
    assert!(text.contains("[Marketplace (2)]"), "{text}");
    assert!(text.contains("install"), "{text}");
    browser.handle_input(keys::ENTER);
    assert!(render_text(&mut browser, 120).contains("→ Install"));
    browser.handle_input(keys::ENTER);
    assert_eq!(calls.take(), vec![BrowserAction::Install("todo".to_string())]);
    browser.handle_input(keys::SHIFT_TAB);
    assert!(render_text(&mut browser, 120).contains("[Installed (2)]"));
}

#[test]
fn update_and_remove_shortcuts_apply_to_package_rows_only() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    browser.handle_input(keys::DOWN);
    browser.handle_input(&keys::ctrl('u'));
    browser.handle_input(&keys::ctrl('d'));
    assert!(calls.is_empty());
    assert!(!render_text(&mut browser, 120).contains("y confirm"));
    browser.handle_input(keys::UP);
    browser.handle_input(&keys::ctrl('u'));
    assert_eq!(calls.take(), vec![BrowserAction::Update("package:user:git".to_string())]);
}

#[test]
fn remove_waits_for_confirmation() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    browser.handle_input(&keys::ctrl('d'));
    assert!(render_text(&mut browser, 120).contains("Remove git-helper? y confirm"));
    browser.handle_input("n");
    assert!(calls.is_empty());
    assert!(!render_text(&mut browser, 120).contains("y confirm"));
    browser.handle_input(&keys::ctrl('d'));
    browser.handle_input("y");
    assert_eq!(calls.take(), vec![BrowserAction::Remove("package:user:git".to_string())]);
}

#[test]
fn busy_blocks_mutations_and_close_but_not_navigation() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    browser.handle_input(&keys::ctrl('d'));
    browser.set_status("Working".to_string(), true);
    browser.handle_input("y");
    browser.handle_input(keys::ESCAPE);
    browser.handle_input(keys::SPACE);
    browser.handle_input(keys::ESCAPE);
    browser.offer_reload();
    browser.handle_input(&keys::ctrl('r'));
    assert!(calls.is_empty());
    browser.handle_input(keys::DOWN);
    browser.handle_input(keys::ENTER);
    assert!(render_text(&mut browser, 120).contains("Path: /home/user/.pi/extensions/linter"));
}

#[test]
fn offline_catalog_keeps_installed_actions_working() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    browser.handle_input(keys::TAB);
    browser.set_catalog(Vec::new(), Some("offline".to_string()));
    browser.handle_input(keys::TAB);
    browser.handle_input(keys::SPACE);
    assert_eq!(calls.take(), vec![BrowserAction::Toggle("package:user:git".to_string())]);
}

#[test]
fn reload_requires_offer() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    browser.handle_input(&keys::ctrl('r'));
    assert!(calls.is_empty());
    browser.offer_reload();
    browser.handle_input(&keys::ctrl('r'));
    assert_eq!(calls.take(), vec![BrowserAction::Reload]);
    assert!(!render_text(&mut browser, 120).contains("Ctrl+R to reload"));
}
