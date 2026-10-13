//! Port of upstream `tree-selector.test.ts`.

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::Component;
use pi_tui::utils::visible_width;
use serde_json::json;

use super::*;
use crate::interactive_components::test_support::{keys, plain, setup};
use crate::interactive_components::tree_selector::FilterMode;
use crate::session::format_iso;

#[test]
fn focuses_nearest_visible_ancestor_when_leaf_is_a_model_change_with_sibling_branch() {
    let _kb = setup();
    let tree = build_tree(&[
        user_message("user-1", None, "hello"),
        assistant_message("asst-1", Some("user-1"), "hi"),
        user_message("user-2", Some("asst-1"), "active branch"),
        model_change("model-1", Some("user-2")),
        user_message("user-3", Some("asst-1"), "sibling branch"),
    ]);
    let selector = selector(&tree, "model-1");
    assert_eq!(selected_id(&selector).as_deref(), Some("user-2"));
}

#[test]
fn hides_context_edits_by_default_and_labels_them_in_all_mode() {
    let _kb = setup();
    let tree = build_tree(&[
        user_message("user-1", None, "hello"),
        assistant_message("asst-1", Some("user-1"), "hi"),
        with("context_edit", "edit-1", Some("asst-1"), json!({"targetId": "asst-1", "replacement": null})),
    ]);
    let default_selector = selector(&tree, "edit-1");
    assert_eq!(selected_id(&default_selector).as_deref(), Some("asst-1"));

    let mut all_selector =
        TreeSelector::new(tree, Some("edit-1"), 24, |_: &str| {}, || {}, None, None, Some(FilterMode::All));
    let rendered = plain(&all_selector.tree_list_mut().render(200)).join("\n");
    assert!(rendered.contains("[context omit: asst-1]"), "{rendered}");
}

#[test]
fn focuses_nearest_visible_ancestor_when_leaf_is_a_thinking_level_change() {
    let _kb = setup();
    let tree = build_tree(&[
        user_message("user-1", None, "hello"),
        assistant_message("asst-1", Some("user-1"), "hi"),
        user_message("user-2", Some("asst-1"), "active branch"),
        with("thinking_level_change", "thinking-1", Some("user-2"), json!({"thinkingLevel": "high"})),
        user_message("user-3", Some("asst-1"), "sibling branch"),
    ]);
    let selector = selector(&tree, "thinking-1");
    assert_eq!(selected_id(&selector).as_deref(), Some("user-2"));
}

fn two_branch_tree() -> TreeData {
    build_tree(&[
        user_message("user-1", None, "hello"),
        assistant_message("asst-1", Some("user-1"), "hi"),
        user_message("user-2", Some("asst-1"), "active branch"),
        assistant_message("asst-2", Some("user-2"), "response"),
        user_message("user-3", Some("asst-1"), "sibling branch"),
    ])
}

#[test]
fn switches_to_nearest_visible_user_message_when_changing_to_user_only_filter() {
    let _kb = setup();
    let mut selector = selector(&two_branch_tree(), "asst-2");
    assert_eq!(selected_id(&selector).as_deref(), Some("asst-2"));
    selector.handle_input(&keys::ctrl('u'));
    assert_eq!(selected_id(&selector).as_deref(), Some("user-2"));
}

#[test]
fn returns_to_nearest_visible_ancestor_when_switching_back_to_default_filter() {
    let _kb = setup();
    let mut selector = selector(&two_branch_tree(), "asst-2");
    assert_eq!(selected_id(&selector).as_deref(), Some("asst-2"));
    selector.handle_input(&keys::ctrl('u'));
    assert_eq!(selected_id(&selector).as_deref(), Some("user-2"));
    selector.handle_input(&keys::ctrl('d'));
    assert_eq!(selected_id(&selector).as_deref(), Some("user-2"));
}

#[test]
fn renders_semantic_help_rows_without_truncating_narrow_terminal_controls() {
    let _kb = setup();
    let tree = build_tree(&[user_message("user-1", None, "hello"), assistant_message("asst-1", Some("user-1"), "hi")]);
    let mut selector = selector(&tree, "asst-1");
    let lines = plain(&selector.render(30));
    let text = lines.join("\n");
    for word in ["branch", "copy", "filters", "cycle", "label time"] {
        assert!(text.contains(word), "missing {word}: {text}");
    }
    assert!(!text.contains("..."), "{text}");
    assert!(lines.iter().all(|line| visible_width(line) <= 30), "{text}");
}

#[test]
fn copies_the_full_selected_message_with_ctrl_x() {
    let _kb = setup();
    let message = format!("{}\nsecond line", "long message ".repeat(30));
    let tree =
        build_tree(&[user_message("user-1", None, "hello"), assistant_message("asst-1", Some("user-1"), &message)]);
    let mut selector = selector(&tree, "asst-1");
    let copied: Rc<RefCell<Option<Option<String>>>> = Rc::default();
    let sink = copied.clone();
    selector.set_on_copy(move |text| *sink.borrow_mut() = Some(text.map(String::from)));
    selector.handle_input(&keys::ctrl('x'));
    assert_eq!(copied.borrow().clone(), Some(Some(message)));
}

#[test]
fn toggles_label_timestamps_for_labeled_nodes() {
    let _kb = setup();
    let mut tree =
        build_tree(&[user_message("user-1", None, "hello"), assistant_message("asst-1", Some("user-1"), "hi")]);
    let local = jiff::civil::date(2026, 3, 28).at(14, 32, 0, 0).to_zoned(jiff::tz::TimeZone::system()).unwrap();
    tree.nodes[0].label = Some("checkpoint".into());
    tree.nodes[0].label_timestamp = Some(format_iso(local.timestamp().as_millisecond()));
    let mut selector = selector(&tree, "asst-1");
    let render = plain(&selector.tree_list_mut().render(200)).join("\n");
    assert!(render.contains("[checkpoint]"), "{render}");
    assert!(!render.contains("3/28 14:32"));
    assert!(!render.contains("[+label time]"));

    selector.handle_input("T");
    let render = plain(&selector.tree_list_mut().render(200)).join("\n");
    assert!(render.contains("3/28 14:32"), "{render}");
    assert!(render.contains("[+label time]"));
}

#[test]
fn preserves_selection_when_switching_to_empty_labeled_filter_and_back() {
    let _kb = setup();
    let tree = build_tree(&[
        user_message("user-1", None, "hello"),
        assistant_message("asst-1", Some("user-1"), "hi"),
        user_message("user-2", Some("asst-1"), "bye"),
        assistant_message("asst-2", Some("user-2"), "goodbye"),
    ]);
    let mut selector = selector(&tree, "asst-2");
    assert_eq!(selected_id(&selector).as_deref(), Some("asst-2"));
    selector.handle_input(&keys::ctrl('l'));
    assert_eq!(selected_id(&selector), None);
    selector.handle_input(&keys::ctrl('d'));
    assert_eq!(selected_id(&selector).as_deref(), Some("asst-2"));
}

#[test]
fn preserves_selection_through_multiple_empty_filter_switches() {
    let _kb = setup();
    let tree = build_tree(&[user_message("user-1", None, "hello"), assistant_message("asst-1", Some("user-1"), "hi")]);
    let mut selector = selector(&tree, "asst-1");
    assert_eq!(selected_id(&selector).as_deref(), Some("asst-1"));
    selector.handle_input(&keys::ctrl('l'));
    assert_eq!(selected_id(&selector), None);
    selector.handle_input(&keys::ctrl('l'));
    assert_eq!(selected_id(&selector).as_deref(), Some("asst-1"));
    selector.handle_input(&keys::ctrl('l'));
    assert_eq!(selected_id(&selector), None);
    selector.handle_input(&keys::ctrl('d'));
    assert_eq!(selected_id(&selector).as_deref(), Some("asst-1"));
}

/// Sends each key and checks the selected id after it.
fn steps(selector: &mut TreeSelector, steps: &[(&str, &str)]) {
    for (index, (key, expected)) in steps.iter().enumerate() {
        selector.handle_input(key);
        assert_eq!(selected_id(selector).as_deref(), Some(*expected), "step {index} ({key:?})");
    }
}

/// Presses down until `id` is selected.
fn move_down_to(selector: &mut TreeSelector, id: &str) {
    for _ in 0..20 {
        selector.handle_input(keys::DOWN);
        if selected_id(selector).as_deref() == Some(id) {
            return;
        }
    }
    panic!("never reached {id}");
}

#[test]
fn ctrl_right_unfolds_a_folded_node_then_jumps_segments_when_unfolded() {
    let _kb = setup();
    let mut selector = selector(&branching_tree(), "asst-4a");
    let (left, right) = (keys::CTRL_LEFT, keys::CTRL_RIGHT);
    steps(
        &mut selector,
        &[
            (left, "user-3a"),
            (left, "user-3a"),
            (keys::DOWN, "user-3b"),
            (keys::UP, "user-3a"),
            (right, "user-3a"),
            (keys::DOWN, "asst-3a"),
            (left, "user-3a"),
            (right, "asst-4a"),
        ],
    );
}

#[test]
fn alt_left_and_right_are_aliases_for_fold_and_unfold_navigation() {
    let _kb = setup();
    let mut selector = selector(&branching_tree(), "asst-4a");
    let (left, right) = (keys::ALT_LEFT, keys::ALT_RIGHT);
    steps(&mut selector, &[(left, "user-3a"), (left, "user-3a"), (right, "user-3a"), (right, "asst-4a")]);
}

#[test]
fn folding_root_hides_entire_subtree_and_nested_fold_is_preserved_on_unfold() {
    let _kb = setup();
    let mut selector = selector(&branching_tree(), "asst-4a");
    let (left, right) = (keys::CTRL_LEFT, keys::CTRL_RIGHT);
    steps(
        &mut selector,
        &[
            (left, "user-3a"),
            (left, "user-3a"),
            (left, "user-1"),
            (left, "user-1"),
            (keys::DOWN, "user-1"),
            (right, "user-1"),
            (right, "user-3a"),
            (keys::DOWN, "user-3b"),
        ],
    );
}

#[test]
fn fold_and_navigate_on_non_active_branch() {
    let _kb = setup();
    let mut selector = selector(&branching_tree(), "asst-4a");
    move_down_to(&mut selector, "user-3b");
    let (left, right) = (keys::CTRL_LEFT, keys::CTRL_RIGHT);
    steps(&mut selector, &[(right, "user-4b"), (left, "user-3b"), (left, "user-3b"), (left, "user-1")]);
}

#[test]
fn fold_and_navigate_with_multiple_roots() {
    let _kb = setup();
    let tree = build_tree(&[
        user_message("user-1", None, "first root"),
        assistant_message("asst-1", Some("user-1"), "response 1"),
        user_message("user-2", None, "second root"),
        assistant_message("asst-2", Some("user-2"), "response 2"),
    ]);
    let mut selector = selector(&tree, "asst-1");
    assert_eq!(selected_id(&selector).as_deref(), Some("asst-1"));
    let (left, right) = (keys::CTRL_LEFT, keys::CTRL_RIGHT);
    steps(
        &mut selector,
        &[
            (left, "user-1"),
            (left, "user-1"),
            (keys::DOWN, "user-2"),
            (right, "asst-2"),
            (left, "user-2"),
            (left, "user-2"),
            (left, "user-2"),
        ],
    );
}

#[test]
fn folding_root_hides_descendants_even_when_intermediate_nodes_are_filtered_out() {
    let _kb = setup();
    let tree = build_tree(&[
        user_message("user-1", None, "hello"),
        tool_call_only_assistant("tool-asst-1", Some("user-1")),
        user_message("user-2", Some("tool-asst-1"), "follow up"),
        assistant_message("asst-2", Some("user-2"), "response"),
    ]);
    let mut selector = selector(&tree, "asst-2");
    steps(&mut selector, &[(keys::CTRL_LEFT, "user-1"), (keys::CTRL_LEFT, "user-1"), (keys::DOWN, "user-1")]);
}

#[test]
fn search_resets_fold_state() {
    let _kb = setup();
    let mut selector = selector(&branching_tree(), "asst-4a");
    selector.handle_input(keys::CTRL_LEFT);
    selector.handle_input(keys::CTRL_LEFT);
    selector.handle_input(keys::DOWN);
    assert_eq!(selected_id(&selector).as_deref(), Some("user-3b"));
    selector.handle_input("b");
    selector.handle_input(keys::ESCAPE);
    move_down_to(&mut selector, "user-3a");
    selector.handle_input(keys::DOWN);
    assert_eq!(selected_id(&selector).as_deref(), Some("asst-3a"));
}

#[test]
fn filter_mode_change_resets_fold_state() {
    let _kb = setup();
    let mut selector = selector(&branching_tree(), "asst-4a");
    selector.handle_input(keys::CTRL_LEFT);
    selector.handle_input(keys::CTRL_LEFT);
    selector.handle_input(&keys::ctrl('u'));
    selector.handle_input(&keys::ctrl('d'));
    move_down_to(&mut selector, "user-3a");
    selector.handle_input(keys::DOWN);
    assert_eq!(selected_id(&selector).as_deref(), Some("asst-3a"));
}
