//! Show-images, theme, and thinking selectors; ports `thinking-selector.test.ts`.

use pi_ai::ThinkingLevel;
use pi_tui::Component;

use crate::interactive_components::test_support::{Calls, Harness, keys, plain, render_text, setup, setup_with};
use crate::interactive_components::{ShowImagesSelector, ThemeSelector, ThinkingSelector};

#[test]
fn show_images_selector_preselects_and_reports_choice() {
    let chosen = Calls::new();
    let cancelled = Calls::new();
    let harness = Harness::new(80, 12, |_| ShowImagesSelector::new(false, chosen.recorder(), cancelled.counter()));
    let screen = harness.screen();
    assert!(screen.contains("→ No"), "{screen}");
    assert!(screen.contains("Show images inline in terminal"));
    assert!(harness.viewport()[0].starts_with("────"));
    harness.send(keys::UP);
    assert!(harness.screen().contains("→ Yes"));
    harness.send(keys::ENTER);
    assert_eq!(chosen.get(), [true]);
    harness.send(keys::ESCAPE);
    assert_eq!(cancelled.len(), 1);
}

#[test]
fn theme_selector_marks_current_and_previews_on_move() {
    let selected = Calls::new();
    let previews = Calls::new();
    let themes = vec!["dark".to_string(), "light".to_string(), "system".to_string()];
    let (on_select, on_preview) = (selected.recorder(), previews.recorder());
    let harness = Harness::new(80, 12, move |_| {
        let mut on_select = on_select;
        let mut on_preview = on_preview;
        ThemeSelector::with_themes(
            themes,
            "light",
            move |name: &str| on_select(name.to_string()),
            || {},
            move |name: &str| on_preview(name.to_string()),
        )
    });
    let screen = harness.screen();
    assert!(screen.contains("→ light"), "{screen}");
    assert!(screen.contains("(current)"));
    harness.send(keys::DOWN);
    assert_eq!(previews.get(), ["system"]);
    harness.send(keys::DOWN);
    assert_eq!(previews.get(), ["system", "dark"]);
    harness.send(keys::ENTER);
    assert_eq!(selected.get(), ["dark"]);
}

#[test]
fn theme_selector_lists_available_themes() {
    let _kb = setup();
    let mut selector = ThemeSelector::new("dark", |_: &str| {}, || {}, |_: &str| {});
    let text = render_text(&mut selector, 80);
    assert!(text.contains("dark") && text.contains("light"), "{text}");
}

fn level_row(selector: &ThinkingSelector, level: &str) -> Option<String> {
    let lines = selector.select_list().borrow_mut().render(80);
    plain(&lines).into_iter().find(|line| line.contains(level))
}

#[test]
fn thinking_selector_keeps_the_current_level_marked_while_browsing() {
    let _kb = setup();
    let levels = [ThinkingLevel::Medium, ThinkingLevel::High];
    let mut selector = ThinkingSelector::new(ThinkingLevel::Medium, &levels, |_| {}, || {}, None, None);
    let selected = selector.select_list().borrow().get_selected_item().map(|item| item.label.clone());
    assert_eq!(selected.as_deref(), Some("✓ medium"));
    assert!(level_row(&selector, "medium").unwrap().starts_with("→ ✓ medium"));
    selector.handle_input(keys::DOWN);
    assert!(level_row(&selector, "medium").unwrap().starts_with("  ✓ medium"));
    assert!(level_row(&selector, "high").unwrap().starts_with("→   high"));
}

#[test]
fn thinking_selector_uses_the_configured_save_binding() {
    let _kb = setup_with(&[("app.thinking.save", "ctrl+r")]);
    let saved = Calls::new();
    let levels = [ThinkingLevel::Medium, ThinkingLevel::High];
    let mut selector =
        ThinkingSelector::new(ThinkingLevel::Medium, &levels, |_| {}, || {}, Some(Box::new(saved.recorder())), None);
    assert!(render_text(&mut selector, 80).contains("Ctrl+R to set as default"));
    selector.handle_input("\x13");
    assert!(saved.is_empty());
    selector.handle_input("\x12");
    assert_eq!(saved.get(), [ThinkingLevel::Medium]);
}

#[test]
fn thinking_selector_filters_and_selects_through_the_terminal() {
    let selected = Calls::new();
    let levels = ThinkingLevel::ALL;
    let on_select = selected.recorder();
    let harness = Harness::new(100, 30, move |_| {
        ThinkingSelector::new(ThinkingLevel::Off, &levels, on_select, || {}, None, Some(ThinkingLevel::High))
    });
    let screen = harness.screen();
    assert!(screen.contains("Thinking Level"));
    assert!(screen.contains("Shift+Tab cycles thinking levels in-session"));
    assert!(screen.contains("Deep reasoning (~16k tokens) · default"), "{screen}");
    harness.type_text("xhi");
    let screen = harness.screen();
    assert!(screen.contains("→   xhigh"), "{screen}");
    assert!(!screen.contains("minimal"));
    harness.send(keys::ENTER);
    assert_eq!(selected.get(), [ThinkingLevel::Xhigh]);
}
