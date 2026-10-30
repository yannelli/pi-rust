use pi_tui::Component;

use super::{BrowserAction, ExtensionRow, ExtensionsBrowser};
use crate::interactive_components::test_support::{Calls, keys, render_text, setup};

pub(super) fn row(id: &str, name: &str, enabled: Option<bool>, update: bool) -> ExtensionRow {
    ExtensionRow {
        id: id.to_string(),
        name: name.to_string(),
        description: format!("{name} description"),
        source: format!("npm:{}", id.rsplit(':').next().unwrap_or(id)),
        version: "1.2.0".to_string(),
        path: format!("/home/user/.pi/extensions/{name}"),
        tags: vec!["tools".to_string()],
        enabled,
        installed: enabled.is_some(),
        update_available: update,
        details: vec![format!("Commands: /{name}")],
    }
}

pub(super) fn installed() -> Vec<ExtensionRow> {
    vec![
        row("package:user:git", "git-helper", Some(true), false),
        row("extension:user:lint", "linter", Some(false), true),
    ]
}

pub(super) fn browser() -> (ExtensionsBrowser, Calls<BrowserAction>) {
    let calls = Calls::new();
    (ExtensionsBrowser::new(installed(), calls.recorder()), calls)
}

#[test]
fn renders_installed_rows_with_markers_and_hints() {
    let _keys = setup();
    let (mut browser, _calls) = browser();
    let text = render_text(&mut browser, 120);
    assert!(text.contains("[Installed (2)]"), "{text}");
    assert!(text.contains("Marketplace"), "{text}");
    assert!(text.contains("→ ● git-helper v1.2.0 [npm:git]"), "{text}");
    assert!(text.contains("○ linter v1.2.0 [npm:lint] disabled update available"), "{text}");
    assert!(text.contains("linter description"), "{text}");
    assert!(text.contains("toggle"), "{text}");
    assert!(text.contains("remove"), "{text}");
    assert!(!text.contains("reload"), "{text}");
}

#[test]
fn renders_details_with_path_tags_details_and_actions() {
    let _keys = setup();
    let (mut browser, _calls) = browser();
    browser.handle_input(keys::DOWN);
    browser.handle_input(keys::ENTER);
    let text = render_text(&mut browser, 120);
    assert!(text.contains("Path: /home/user/.pi/extensions/linter"), "{text}");
    assert!(text.contains("Tags: tools"), "{text}");
    assert!(text.contains("Status: installed, disabled"), "{text}");
    assert!(text.contains("Commands: /linter"), "{text}");
    assert!(text.contains("→ Enable"), "{text}");
    assert!(!text.contains("Remove"), "{text}");
}

#[test]
fn package_details_offer_toggle_update_and_remove() {
    let _keys = setup();
    let (mut browser, _calls) = browser();
    browser.handle_input(keys::ENTER);
    let text = render_text(&mut browser, 120);
    assert!(text.contains("→ Disable"), "{text}");
    assert!(text.contains("   Update"), "{text}");
    assert!(text.contains("   Remove"), "{text}");
}

#[test]
fn renders_marketplace_loading_then_error_inline() {
    let _keys = setup();
    let (mut browser, _calls) = browser();
    browser.handle_input(keys::TAB);
    assert!(render_text(&mut browser, 120).contains("Loading marketplace"));
    browser.set_catalog(Vec::new(), Some("network down".to_string()));
    let text = render_text(&mut browser, 120);
    assert!(text.contains("[Marketplace (0)]"), "{text}");
    assert!(text.contains("Marketplace unavailable: network down"), "{text}");
    assert!(text.contains("Installed extensions still work"), "{text}");
}

#[test]
fn renders_busy_status_and_reload_offer() {
    let _keys = setup();
    let (mut browser, _calls) = browser();
    browser.set_status("Installing todo".to_string(), true);
    browser.offer_reload();
    let text = render_text(&mut browser, 120);
    assert!(text.contains("⋯ Installing todo"), "{text}");
    assert!(text.contains("busy: changes disabled"), "{text}");
    assert!(text.contains("Ctrl+R to reload"), "{text}");
    browser.set_status("Installed todo".to_string(), false);
    let text = render_text(&mut browser, 120);
    assert!(text.contains("Installed todo"), "{text}");
    assert!(!text.contains("busy"), "{text}");
}

#[test]
fn render_fits_narrow_width() {
    let _keys = setup();
    let (mut browser, _calls) = browser();
    for line in browser.render(30) {
        assert!(pi_tui::utils::visible_width(&line) <= 30, "{line:?}");
    }
}

#[test]
fn long_errors_wrap_without_losing_the_reason() {
    let _keys = setup();
    let (mut browser, _) = browser();
    browser.set_status(format!("{} SHA-256 mismatch", "download failed ".repeat(8)), false);
    assert!(
        render_text(&mut browser, 40).split_whitespace().collect::<Vec<_>>().join(" ").contains("SHA-256 mismatch")
    );
    browser.set_status(String::new(), false);
    browser.set_catalog(Vec::new(), Some(format!("{} invalid signature", "index rejected ".repeat(8))));
    browser.handle_input(keys::TAB);
    assert!(
        render_text(&mut browser, 40).split_whitespace().collect::<Vec<_>>().join(" ").contains("invalid signature")
    );
}
