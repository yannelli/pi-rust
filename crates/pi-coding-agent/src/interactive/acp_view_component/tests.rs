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
    for label in
        ["Log in", "Set as default", "Disable", "Move to project settings", "Edit", "Remove", "Provider: kiro-acp"]
    {
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

fn add_form(browser: &mut AcpBrowser, name: &str, command: &str) {
    browser.handle_input("\x0e");
    browser.handle_input(name);
    browser.handle_input(keys::TAB);
    browser.handle_input(keys::TAB);
    browser.handle_input(command);
}

#[test]
fn add_row_and_shortcut_open_form_and_cancel_returns_to_list() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    let text = render_text(&mut browser, 120);
    assert!(text.contains("Add custom agent") && text.contains("ctrl+n"), "{text}");
    browser.handle_input(keys::UP);
    browser.handle_input(keys::ENTER);
    assert!(render_text(&mut browser, 120).contains("Name:"));
    browser.handle_input("\x13");
    assert!(render_text(&mut browser, 120).contains("Name is required."));
    assert!(calls.is_empty());
    browser.handle_input(keys::ESCAPE);
    add_form(&mut browser, "My Agent", "agent-cli");
    let text = render_text(&mut browser, 120);
    assert!(text.contains("my-agent-acp") && text.contains("Enabled: on"), "{text}");
    browser.handle_input("\x13");
    let actions = calls.take();
    let [AcpAction::Save { entry, scope, editing }] = actions.as_slice() else { panic!("{actions:?}") };
    assert_eq!(
        (entry.id.as_str(), entry.source.as_str(), *scope, *editing),
        ("my-agent", "custom", AcpScope::Global, false)
    );
}

#[test]
fn validation_retains_input_for_empty_command_duplicate_id_and_bad_environment() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    add_form(&mut browser, "My Agent", "");
    browser.handle_input("\x13");
    let text = render_text(&mut browser, 120);
    assert!(text.contains("Command is required.") && text.contains("My Agent"), "{text}");
    assert!(calls.is_empty());
    browser.handle_input("agent-cli");
    browser.handle_input("\x1b[Z");
    browser.handle_input("\x0b");
    browser.handle_input("kiro");
    browser.handle_input("\x13");
    assert!(render_text(&mut browser, 120).contains("already installed"));
    assert!(calls.is_empty());
    browser.handle_input("\x01");
    browser.handle_input("\x0b");
    browser.handle_input("kiro-acp");
    browser.handle_input("\x13");
    assert!(render_text(&mut browser, 120).contains("Provider \"kiro-acp\" already belongs to"));
    assert!(calls.is_empty());
    browser.handle_input("\x01");
    browser.handle_input("\x0b");
    browser.handle_input("custom");
    for _ in 0..5 {
        browser.handle_input(keys::TAB);
    }
    browser.handle_input("INVALID-LINE");
    browser.handle_input("\x13");
    let text = render_text(&mut browser, 120);
    assert!(text.contains("Environment line 1") && text.contains("INVALID-LINE"), "{text}");
    assert!(calls.is_empty());
    browser.handle_input("\x01");
    browser.handle_input("\x0b");
    browser.handle_input("TOKEN=secret=with=equals");
    browser.handle_input("\x13");
    let actions = calls.take();
    let [AcpAction::Save { entry, .. }] = actions.as_slice() else { panic!("{actions:?}") };
    assert_eq!(entry.env["TOKEN"], "secret=with=equals");
}

#[test]
fn invalid_id_and_shell_words_stay_in_form() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    add_form(&mut browser, "My Agent", "agent-cli");
    browser.handle_input("\x1b[Z");
    browser.handle_input("\x0b");
    browser.handle_input("Bad_ID");
    browser.handle_input("\x13");
    assert!(render_text(&mut browser, 120).contains("lowercase letters, digits, and dashes"));
    browser.handle_input("\x01");
    browser.handle_input("\x0b");
    browser.handle_input("my-agent");
    browser.handle_input(keys::TAB);
    browser.handle_input(keys::TAB);
    browser.handle_input("'unclosed");
    browser.handle_input("\x13");
    assert!(render_text(&mut browser, 120).contains("Arguments: Unclosed quote"));
    assert!(calls.is_empty());
}

#[test]
fn save_error_preserves_form_and_busy_blocks_changes() {
    let _keys = setup();
    let (mut browser, calls) = browser();
    add_form(&mut browser, "My Agent", "agent-cli");
    browser.handle_input("\x13");
    calls.take();
    browser.handle_input("\x13");
    browser.handle_input(keys::ESCAPE);
    assert!(calls.is_empty());
    browser.finish_save(Some("Project settings are not trusted".into()));
    browser.set_status(String::new(), false);
    let text = render_text(&mut browser, 120);
    assert!(text.contains("My Agent") && text.contains("Project settings are not trusted"), "{text}");
    browser.handle_input("\x13");
    assert_eq!(calls.take().len(), 1);
}
