//! Ports `scoped-models-selector.test.ts`, plus terminal tests for the `app.models.*` actions.

use std::cell::RefCell;
use std::rc::Rc;

use pi_ai::Model;
use pi_tui::Component;

use crate::interactive_components::scoped_models_selector::{
    EnabledIds, ModelsCallbacks, ModelsConfig, RefreshStatusKind, ScopedModelsSelector,
};
use crate::interactive_components::test_support::{Calls, Harness, keys, plain, setup};

fn model(id: &str, provider: &str) -> Model {
    Model { id: id.into(), name: format!("Model {id}"), provider: provider.into(), ..Default::default() }
}

struct State {
    ids: Vec<&'static str>,
    enabled: Rc<RefCell<Vec<bool>>>,
}

fn create(models: &[(&'static str, bool)]) -> (ScopedModelsSelector, State) {
    let ids: Vec<&'static str> = models.iter().map(|(id, _)| *id).collect();
    let enabled = Rc::new(RefCell::new(models.iter().map(|(_, on)| *on).collect::<Vec<_>>()));
    let all_models = ids.iter().map(|id| model(id, "faux")).collect();
    let initial = models.iter().filter(|(_, on)| *on).map(|(id, _)| format!("faux/{id}")).collect();
    let (state_ids, state) = (ids.clone(), enabled.clone());
    let selector = ScopedModelsSelector::new(
        ModelsConfig { all_models, enabled_model_ids: Some(initial), refresh_status: None },
        ModelsCallbacks {
            on_change: Box::new(move |enabled: EnabledIds| {
                let flags = state_ids
                    .iter()
                    .map(|id| enabled.as_ref().is_none_or(|ids| ids.contains(&format!("faux/{id}"))))
                    .collect();
                *state.borrow_mut() = flags;
            }),
            on_persist: Box::new(|_| {}),
            on_cancel: Box::new(|| {}),
        },
    );
    (selector, State { ids, enabled })
}

fn marker_states(selector: &mut ScopedModelsSelector, state: &State) -> Vec<bool> {
    let lines = plain(&selector.render(120));
    state
        .ids
        .iter()
        .map(|id| {
            let line = lines.iter().find(|line| line.contains(&format!("{id} ["))).expect("row");
            line.chars().skip(2).collect::<String>().starts_with("✓ ")
        })
        .collect()
}

fn text(selector: &mut ScopedModelsSelector) -> String {
    plain(&selector.render(120)).join("\n")
}

#[test]
fn marks_every_model_after_enabling_all() {
    let _kb = setup();
    let (mut selector, state) = create(&[("model-a", true), ("model-b", false), ("model-c", false)]);
    selector.handle_input("\x01");
    assert_eq!(*state.enabled.borrow(), [true, true, true]);
    assert_eq!(marker_states(&mut selector, &state), [true, true, true]);
    assert!(text(&mut selector).contains("all enabled"));
}

#[test]
fn disables_only_the_selected_model_after_enabling_all() {
    let _kb = setup();
    let (mut selector, state) = create(&[("model-a", true), ("model-b", false), ("model-c", false)]);
    selector.handle_input("\x01");
    selector.handle_input("\r");
    assert_eq!(*state.enabled.borrow(), [false, true, true]);
    assert_eq!(marker_states(&mut selector, &state), [false, true, true]);
}

#[test]
fn enables_only_the_selected_model_after_clearing_all() {
    let _kb = setup();
    let (mut selector, state) = create(&[("model-a", true), ("model-b", true), ("model-c", true)]);
    selector.handle_input("\x18");
    assert_eq!(*state.enabled.borrow(), [false, false, false]);
    assert_eq!(marker_states(&mut selector, &state), [false, false, false]);
    selector.handle_input("\r");
    assert_eq!(*state.enabled.borrow(), [true, false, false]);
    assert_eq!(marker_states(&mut selector, &state), [true, false, false]);
}

#[test]
fn restores_the_all_enabled_state_after_re_enabling_the_last_disabled_model() {
    let _kb = setup();
    let (mut selector, state) = create(&[("model-a", true), ("model-b", false), ("model-c", false)]);
    selector.handle_input("\x01");
    selector.handle_input("\r");
    selector.handle_input(keys::DOWN);
    selector.handle_input(keys::DOWN);
    selector.handle_input("\r");
    assert_eq!(*state.enabled.borrow(), [true, true, true]);
    assert_eq!(marker_states(&mut selector, &state), [true, true, true]);
    assert!(text(&mut selector).contains("all enabled"));
}

fn harness(persisted: &Calls<EnabledIds>, cancelled: &Calls<()>) -> Harness<ScopedModelsSelector> {
    let all_models = vec![model("a1", "alpha"), model("a2", "alpha"), model("b1", "beta")];
    let enabled = Some(vec!["alpha/a1".to_string(), "gone/old".to_string()]);
    let (persisted, cancelled) = (persisted.recorder(), cancelled.counter());
    Harness::new(140, 30, move |_| {
        ScopedModelsSelector::new(
            ModelsConfig { all_models, enabled_model_ids: enabled, refresh_status: Some("Refreshing…".into()) },
            ModelsCallbacks {
                on_change: Box::new(|_| {}),
                on_persist: Box::new(persisted),
                on_cancel: Box::new(cancelled),
            },
        )
    })
}

#[test]
fn shows_unavailable_models_footer_and_saves() {
    let (persisted, cancelled) = (Calls::new(), Calls::new());
    let harness = harness(&persisted, &cancelled);
    let screen = harness.screen();
    assert!(screen.contains("Model Configuration"), "{screen}");
    assert!(screen.contains("Session-only. Ctrl+S to save to settings."));
    assert!(screen.contains("→ ✓ a1 [alpha]"));
    assert!(screen.contains("    gone/old [unavailable]"), "{screen}");
    assert!(screen.contains("  Refreshing…"));
    // Alt shows as Option on macOS, as upstream `formatKeyText` does.
    let alt = if cfg!(target_os = "macos") { "Option" } else { "Alt" };
    let footer = format!(
        "Enter toggle · Ctrl+A all · Ctrl+X clear · Ctrl+P provider · {alt}+Up/{alt}+Down reorder · Ctrl+S save · 1/3 enabled · 1 unavailable"
    );
    assert!(screen.contains(&footer), "{screen}");
    harness.send(&keys::ctrl('p'));
    assert!(harness.screen().contains("(unsaved)"));
    assert!(harness.screen().contains("2/3 enabled"));
    harness.send(&keys::ctrl('s'));
    assert_eq!(persisted.get(), [Some(vec!["alpha/a1".into(), "gone/old".into(), "alpha/a2".into()])]);
    assert!(!harness.screen().contains("(unsaved)"));
    harness.component.borrow_mut().set_refresh_status("Model catalogs refreshed.", RefreshStatusKind::Success);
    harness.settle();
    assert!(harness.screen().contains("  Model catalogs refreshed."));
}

#[test]
fn reorders_enabled_models_and_filters_with_search() {
    let (persisted, cancelled) = (Calls::new(), Calls::new());
    let harness = harness(&persisted, &cancelled);
    harness.send(keys::ALT_DOWN);
    harness.send(&keys::ctrl('s'));
    assert_eq!(persisted.take(), [Some(vec!["gone/old".into(), "alpha/a1".into()])]);
    assert!(harness.screen().contains("→ ✓ a1 [alpha]"));
    harness.type_text("b1");
    let screen = harness.screen();
    assert!(screen.contains("→   b1 [beta]") && !screen.contains("a2 [alpha]"), "{screen}");
    harness.send(&keys::ctrl('a'));
    harness.send(&keys::ctrl('s'));
    assert_eq!(persisted.take(), [Some(vec!["gone/old".into(), "alpha/a1".into(), "beta/b1".into()])]);
    harness.send(&keys::ctrl('c'));
    assert!(cancelled.is_empty());
    assert!(harness.screen().contains("a2 [alpha]"));
    harness.send(&keys::ctrl('c'));
    assert_eq!(cancelled.len(), 1);
    harness.send(keys::ESCAPE);
    assert_eq!(cancelled.len(), 2);
}

#[test]
fn update_models_keeps_the_selected_row() {
    let (persisted, cancelled) = (Calls::new(), Calls::new());
    let harness = harness(&persisted, &cancelled);
    harness.send(keys::DOWN);
    harness.send(keys::DOWN);
    harness.component.borrow_mut().update_models(vec![model("b1", "beta"), model("a2", "alpha")], Some(None));
    harness.settle();
    let screen = harness.screen();
    assert!(screen.contains("→ ✓ a2 [alpha]"), "{screen}");
    assert!(screen.contains("all enabled"));
}
