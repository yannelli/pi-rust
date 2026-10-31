use pi_tui::Component;

use super::{AcpAction, AcpBrowser, AcpRow};
use crate::acp_client::AcpScope;
use crate::interactive_components::test_support::{Calls, keys, render_text, setup};

fn installed(id: &str, enabled: bool, is_default: bool) -> AcpRow {
    AcpRow {
        id: id.into(),
        name: format!("{id} agent"),
        version: "1.0.0".into(),
        scope: Some(AcpScope::Global),
        enabled: Some(enabled),
        installed: true,
        is_default,
        details: vec![format!("Provider: {id}-acp")],
        ..AcpRow::default()
    }
}

fn registry(id: &str, installed: bool, builtin: bool) -> AcpRow {
    AcpRow { id: id.into(), name: format!("{id} agent"), installed, builtin, ..AcpRow::default() }
}

fn browser() -> (AcpBrowser, Calls<AcpAction>) {
    let calls = Calls::new();
    let rows = vec![installed("kiro", true, true), installed("gemini", false, false)];
    (AcpBrowser::new(rows, calls.recorder()), calls)
}

#[test]
fn renders_installed_rows_with_scope_default_and_disabled() {
    let _keys = setup();
    let (mut browser, _calls) = browser();
    let text = render_text(&mut browser, 120);
    assert!(text.contains("[Installed (2)]"), "{text}");
    assert!(text.contains("→ ● kiro agent v1.0.0 [global] default"), "{text}");
    assert!(text.contains("○ gemini agent v1.0.0 [global] disabled"), "{text}");
}

#[test]
fn enter_opens_menu_and_runs_items() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    browser.handle_input(keys::ENTER);
    let text = render_text(&mut browser, 120);
    for label in ["Log in", "Set as default", "Disable", "Move to project settings", "Remove", "Provider: kiro-acp"] {
        assert!(text.contains(label), "{label}: {text}");
    }
    browser.handle_input(keys::ENTER);
    assert_eq!(calls.take(), vec![AcpAction::Login("kiro".into())]);
    browser.handle_input(keys::ENTER);
    browser.handle_input(keys::DOWN);
    browser.handle_input(keys::ENTER);
    assert_eq!(calls.take(), vec![AcpAction::SetDefault("kiro".into())]);
    browser.handle_input(keys::DOWN);
    browser.handle_input(keys::ENTER);
    browser.handle_input(keys::DOWN);
    browser.handle_input(keys::DOWN);
    browser.handle_input(keys::ENTER);
    assert_eq!(calls.take(), vec![AcpAction::Toggle("gemini".into(), AcpScope::Global)]);
    assert!(render_text(&mut browser, 120).contains("Enabling gemini agent"));
}

#[test]
fn remove_asks_for_confirmation() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    browser.handle_input(keys::ENTER);
    browser.handle_input(keys::UP);
    browser.handle_input(keys::ENTER);
    assert!(render_text(&mut browser, 120).contains("Remove kiro agent from the global settings? y confirm"));
    browser.handle_input("n");
    assert!(calls.is_empty());
    browser.handle_input(keys::ENTER);
    browser.handle_input(keys::UP);
    browser.handle_input(keys::ENTER);
    browser.handle_input("y");
    assert_eq!(calls.take(), vec![AcpAction::Remove("kiro".into(), AcpScope::Global)]);
}

#[test]
fn registry_tab_installs_and_marks_installed() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    browser.handle_input(keys::TAB);
    assert!(render_text(&mut browser, 120).contains("Loading the ACP registry"));
    let rows = vec![registry("kiro", true, true), registry("amp-acp", false, true), registry("gemini", false, false)];
    browser.set_registry(rows, Some("HTTP 500".into()));
    let text = render_text(&mut browser, 120);
    assert!(text.contains("[Registry (3)]"), "{text}");
    assert!(text.contains("Registry unavailable: HTTP 500"), "{text}");
    assert!(text.contains("✓ kiro agent built-in installed"), "{text}");
    browser.handle_input(keys::ENTER);
    assert!(calls.is_empty());
    assert!(render_text(&mut browser, 120).contains("kiro agent is already installed"));
    browser.handle_input(keys::DOWN);
    browser.handle_input(keys::ENTER);
    let text = render_text(&mut browser, 120);
    assert!(text.contains("Install amp-acp agent in") && text.contains("→ Global settings: every project"), "{text}");
    assert!(calls.is_empty(), "the scope comes first");
    browser.handle_input(keys::DOWN);
    browser.handle_input(keys::ENTER);
    assert_eq!(calls.take(), vec![AcpAction::Install("amp-acp".into(), AcpScope::Project)]);
    assert!(render_text(&mut browser, 120).contains("Installing amp-acp agent in the project settings"));
    browser.set_status(String::new(), false);
    browser.handle_input(keys::ESCAPE);
    assert_eq!(calls.take(), vec![AcpAction::Close]);
}

#[test]
fn shadowed_global_rows_keep_their_own_scope_actions() {
    let _keys = setup();
    let calls = Calls::new();
    let project = AcpRow { scope: Some(AcpScope::Project), ..installed("kiro", true, true) };
    let global = AcpRow { shadowed: true, is_default: false, ..installed("kiro", false, false) };
    let mut browser = AcpBrowser::new(vec![global, project], calls.recorder());
    let text = render_text(&mut browser, 160);
    assert!(text.contains("kiro agent v1.0.0 [global] replaced by the project entry disabled"), "{text}");
    assert!(text.contains("kiro agent v1.0.0 [project] default"), "{text}");
    browser.handle_input(keys::ENTER);
    let menu = render_text(&mut browser, 160);
    assert!(menu.contains("Enable") && menu.contains("Remove") && !menu.contains("Log in"), "{menu}");
    browser.handle_input(keys::ENTER);
    assert_eq!(calls.take(), vec![AcpAction::Toggle("kiro".into(), AcpScope::Global)]);
    browser.set_status(String::new(), false);
    browser.handle_input(keys::DOWN);
    browser.handle_input(keys::ENTER);
    for _ in 0..3 {
        browser.handle_input(keys::DOWN);
    }
    browser.handle_input(keys::ENTER);
    assert_eq!(calls.take(), vec![AcpAction::Move("kiro".into(), AcpScope::Project)]);
}
