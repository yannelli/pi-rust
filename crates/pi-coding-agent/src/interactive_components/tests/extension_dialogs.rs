//! Extension selector, input, and editor; ports `trust-selector.test.ts`.

use std::path::PathBuf;
use std::time::Duration;

use pi_tui::Component;

use crate::interactive_components::extension_editor::{ExtensionEditor, ExtensionEditorOptions};
use crate::interactive_components::extension_input::{ExtensionInput, ExtensionInputOptions};
use crate::interactive_components::extension_selector::{ExtensionSelector, ExtensionSelectorOptions};
use crate::interactive_components::test_support::{Calls, Harness, keys, render_text, setup};
use crate::interactive_components::trust_selector::{TrustSelection, TrustSelector, TrustSelectorOptions};
use crate::resources::trust::{ProjectTrustStoreEntry, ProjectTrustUpdate};

fn options(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

#[test]
fn extension_selector_navigates_and_selects() {
    let selected = Calls::new();
    let cancelled = Calls::new();
    let on_select = selected.recorder();
    let harness = Harness::new(80, 20, |_| {
        let mut on_select = on_select;
        ExtensionSelector::new(
            "Pick one",
            options(&["alpha", "beta", "gamma"]),
            move |option: &str| on_select(option.to_string()),
            cancelled.counter(),
            ExtensionSelectorOptions { description: Some("Some details".into()), ..Default::default() },
        )
    });
    let screen = harness.screen();
    assert!(screen.contains(" Pick one"), "{screen}");
    assert!(screen.contains(" Some details"));
    assert!(screen.contains(" → alpha"));
    assert!(screen.contains("↑↓ navigate  enter select  escape/ctrl+c cancel"));
    harness.send("j");
    assert!(harness.screen().contains(" → beta"));
    harness.send_all(&[keys::DOWN, keys::DOWN, "k"]);
    assert!(harness.screen().contains(" → beta"));
    harness.send("\n");
    assert_eq!(selected.get(), ["beta"]);
    harness.send(keys::ESCAPE);
    assert_eq!(cancelled.len(), 1);
}

#[test]
fn extension_selector_toggles_tools_and_counts_down() {
    let toggled = Calls::new();
    let cancelled = Calls::new();
    let toggle = toggled.counter();
    let harness = Harness::new(80, 20, |tui| {
        ExtensionSelector::new(
            "Confirm",
            options(&["Yes", "No"]),
            |_: &str| {},
            cancelled.counter(),
            ExtensionSelectorOptions {
                tui: Some(tui.clone()),
                timeout: Some(Duration::from_millis(2000)),
                on_toggle_tools_expanded: Some(Box::new(toggle)),
                description: None,
            },
        )
    });
    assert!(harness.screen().contains("Confirm (2s)"));
    harness.send(&keys::ctrl('o'));
    assert_eq!(toggled.len(), 1);
    harness.run_for(Duration::from_millis(1100));
    assert!(harness.screen().contains("Confirm (1s)"));
    harness.run_for(Duration::from_millis(1100));
    assert_eq!(cancelled.len(), 1);
}

#[test]
fn extension_input_submits_typed_value() {
    let submitted = Calls::new();
    let on_submit = submitted.recorder();
    let harness = Harness::new(80, 20, |_| {
        let mut on_submit = on_submit;
        ExtensionInput::new(
            "Name",
            Some("ignored"),
            move |value: &str| on_submit(value.to_string()),
            || {},
            ExtensionInputOptions { initial_value: Some("ab".into()), ..Default::default() },
        )
    });
    let screen = harness.screen();
    assert!(screen.contains(" Name"));
    assert!(screen.contains("> ab"));
    assert!(screen.contains("enter submit  escape/ctrl+c cancel"));
    harness.send(keys::ENTER);
    assert_eq!(submitted.get(), ["ab"]);
    // Upstream keeps the cursor at the start after `setValue`.
    harness.type_text("c");
    harness.send(keys::ENTER);
    assert_eq!(submitted.get(), ["ab", "cab"]);
}

#[test]
fn extension_input_cancels_on_timeout() {
    let cancelled = Calls::new();
    let harness = Harness::new(80, 20, |tui| {
        let opts = ExtensionInputOptions {
            tui: Some(tui.clone()),
            timeout: Some(Duration::from_millis(1000)),
            ..Default::default()
        };
        ExtensionInput::new("Wait", None, |_: &str| {}, cancelled.counter(), opts)
    });
    assert!(harness.screen().contains("Wait (1s)"));
    harness.run_for(Duration::from_millis(1100));
    assert_eq!(cancelled.len(), 1);
}

#[test]
fn extension_editor_submits_and_cancels() {
    let submitted = Calls::new();
    let cancelled = Calls::new();
    let on_submit = submitted.recorder();
    let harness = Harness::new(100, 20, |tui| {
        let mut on_submit = on_submit;
        let options = ExtensionEditorOptions { description: Some("Edit the plan".into()), ..Default::default() };
        ExtensionEditor::new(
            tui,
            "Plan",
            Some("first"),
            move |text: &str| on_submit(text.to_string()),
            cancelled.counter(),
            options,
            Some("true".into()),
        )
    });
    let screen = harness.screen();
    assert!(screen.contains(" Plan") && screen.contains(" Edit the plan"), "{screen}");
    assert!(screen.contains("first"));
    assert!(screen.contains("ctrl+g external editor"), "{screen}");
    harness.type_text(" line");
    harness.send(keys::ENTER);
    assert_eq!(submitted.get(), ["first line"]);
    harness.send(keys::ESCAPE);
    assert_eq!(cancelled.len(), 1);
}

fn trust(cwd: &str, saved: Option<(&str, bool)>, trusted: bool, selections: &Calls<TrustSelection>) -> TrustSelector {
    TrustSelector::new(TrustSelectorOptions {
        cwd: PathBuf::from(cwd),
        saved_decision: saved.map(|(path, decision)| ProjectTrustStoreEntry { path: path.into(), decision }),
        project_trusted: trusted,
        on_select: Box::new(selections.recorder()),
        on_cancel: Box::new(|| {}),
    })
}

fn update(path: &str, decision: Option<bool>) -> ProjectTrustUpdate {
    ProjectTrustUpdate { path: path.into(), decision }
}

#[test]
fn trust_selector_keeps_the_saved_trusted_decision_marked_while_browsing() {
    let _kb = setup();
    let mut selector = trust("/project", Some(("/project", true)), true, &Calls::new());
    let output = render_text(&mut selector, 120);
    assert!(output.contains("Saved decision: trusted (/project)"), "{output}");
    assert!(output.contains("Current session: trusted"));
    assert!(output.contains("→ ✓ Trust"));
    selector.handle_input(keys::DOWN);
    let output = render_text(&mut selector, 120);
    assert!(output.contains("✓ Trust"));
    assert!(output.contains("→   Trust parent folder (/)"));
    assert!(!output.contains("✓ Do not trust"));
}

#[test]
fn trust_selector_selects_a_trust_decision() {
    let _kb = setup();
    let selections = Calls::new();
    let mut selector = trust("/project", None, false, &selections);
    selector.handle_input("\n");
    assert_eq!(selections.get(), [TrustSelection { trusted: true, updates: vec![update("/project", Some(true))] }]);
}

#[test]
fn trust_selector_labels_saved_ancestor_decisions_as_inherited() {
    let _kb = setup();
    let mut selector = trust("/parent/project/nested", Some(("/parent", true)), true, &Calls::new());
    let output = render_text(&mut selector, 120);
    assert!(output.contains("Saved decision: trusted (inherited from /parent)"), "{output}");
}

#[test]
fn trust_selector_adds_a_trust_parent_option() {
    let selections = Calls::new();
    let harness = Harness::new(120, 20, |_| trust("/parent/project", Some(("/parent", true)), true, &selections));
    let screen = harness.screen();
    assert!(screen.contains("Saved decision: trusted (inherited from /parent)"), "{screen}");
    assert!(screen.contains("✓ Trust parent folder (/parent)"));
    harness.send(keys::ENTER);
    let expected = vec![update("/parent", Some(true)), update("/parent/project", None)];
    assert_eq!(selections.get(), [TrustSelection { trusted: true, updates: expected }]);
}
