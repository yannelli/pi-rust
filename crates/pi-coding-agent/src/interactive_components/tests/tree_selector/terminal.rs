//! The tree selector driven through a VirtualTerminal: rendering, navigation, search, filters,
//! folding, label editing, and callbacks.

use std::time::Duration;

use super::*;
use crate::interactive_components::test_support::{Calls, Harness, keys};
use crate::interactive_components::tree_selector::FilterMode;

const SHIFT_CTRL_O: &str = "\x1b[111;6u";

fn mount(tree: TreeData, leaf: &str, selected: &Calls<String>, cancelled: &Calls<()>) -> Harness<TreeSelector> {
    let (mut on_select, on_cancel) = (selected.recorder(), cancelled.counter());
    let leaf = leaf.to_string();
    Harness::new(100, 40, move |_| {
        TreeSelector::new(tree, Some(&leaf), 24, move |id: &str| on_select(id.to_string()), on_cancel, None, None, None)
    })
}

/// The `(n/m)` status row.
fn status<C: pi_tui::Component>(harness: &Harness<C>) -> String {
    let screen = harness.viewport();
    let found = screen.iter().find(|line| line.starts_with("  (") && line.contains('/') && line.contains(')'));
    found.cloned().unwrap_or_else(|| panic!("no status row:\n{}", screen.join("\n")))
}

/// Visible rows that hold `text`.
fn row<C: pi_tui::Component>(harness: &Harness<C>, text: &str) -> String {
    let screen = harness.viewport();
    screen.iter().find(|line| line.contains(text)).cloned().unwrap_or_else(|| panic!("{text}:\n{}", screen.join("\n")))
}

#[test]
fn renders_header_tree_connectors_active_path_and_position() {
    let (selected, cancelled) = (Calls::new(), Calls::new());
    let harness = mount(branching_tree(), "asst-4a", &selected, &cancelled);
    let screen = harness.viewport();
    assert_eq!(screen[0], "");
    assert!(screen[1].starts_with("────"));
    assert_eq!(screen[2], "   Session Tree");
    let text = harness.screen();
    // macOS lists option+left/right first for branch folding, and shows alt as option.
    let branch = if cfg!(target_os = "macos") { "option" } else { "ctrl" };
    assert!(text.contains(&format!("  ↑/↓ move · ←/→ page · {branch}+←/→ branch · ctrl+x copy")), "{text}");
    assert!(text.contains("filters ctrl+d/t/u/l/a"), "{text}");
    assert!(text.contains("cycle ctrl+o/shift+ctrl+o"), "{text}");
    assert!(text.contains("  Type to search:"));
    let expected = [
        "  • user: first message",
        "  • assistant: response 1",
        "  • user: second message",
        "  • assistant: response 2",
        "  ├⊟ • user: branch A start",
        "  │     • assistant: branch A response",
        "  │     • user: branch A deep",
        "› │     • assistant: branch A leaf",
        "  └⊟ user: branch B start",
        "        assistant: branch B response",
        "        user: branch B deep",
        "  (8/11)",
    ];
    let start = screen.iter().position(|line| line == expected[0]).unwrap_or_else(|| panic!("{text}"));
    assert_eq!(&screen[start..start + expected.len()], expected, "{text}");
    assert_eq!(screen[start + expected.len()], "");
    assert!(screen[start + expected.len() + 1].starts_with("────"));
}

#[test]
fn moves_pages_wraps_and_reports_the_selected_entry() {
    let (selected, cancelled) = (Calls::new(), Calls::new());
    let harness = mount(branching_tree(), "asst-4a", &selected, &cancelled);
    harness.send(keys::DOWN);
    assert!(row(&harness, "branch B start").starts_with("› "));
    harness.send_all(&[keys::DOWN, keys::DOWN, keys::DOWN]);
    assert!(row(&harness, "first message").starts_with("› "), "down wraps to the top");
    harness.send(keys::UP);
    assert!(row(&harness, "branch B deep").starts_with("› "), "up wraps to the bottom");
    harness.send(keys::LEFT);
    assert!(row(&harness, "first message").starts_with("› "), "page up stops at the top");
    harness.send(keys::RIGHT);
    assert!(row(&harness, "branch B deep").starts_with("› "), "page down stops at the bottom");
    harness.send(keys::PAGE_UP);
    assert!(row(&harness, "first message").starts_with("› "));
    harness.send(keys::PAGE_DOWN);
    harness.send(keys::ENTER);
    assert_eq!(selected.get(), ["user-4b"]);
    assert!(cancelled.is_empty());
}

#[test]
fn search_filters_rows_and_escape_clears_before_cancelling() {
    let (selected, cancelled) = (Calls::new(), Calls::new());
    let harness = mount(branching_tree(), "asst-4a", &selected, &cancelled);
    harness.type_text("response");
    let text = harness.screen();
    assert!(text.contains("  Type to search: response"), "{text}");
    assert!(!text.contains("branch A start"), "{text}");
    assert_eq!(status(&harness), "  (3/4)", "the nearest visible ancestor stays selected");
    assert!(row(&harness, "branch A response").starts_with("› "));
    harness.type_text(" 2");
    assert_eq!(status(&harness), "  (1/1)");
    assert!(row(&harness, "assistant: response 2").starts_with("› "));
    harness.send_all(&[keys::BACKSPACE, keys::BACKSPACE]);
    assert!(harness.screen().contains("  Type to search: response\n"));
    assert_eq!(status(&harness), "  (2/4)");
    harness.send(keys::ESCAPE);
    assert!(harness.screen().contains("  Type to search:\n"));
    assert_eq!(status(&harness), "  (4/11)");
    assert!(cancelled.is_empty());
    harness.send(keys::ESCAPE);
    assert_eq!(cancelled.len(), 1);
    assert!(selected.is_empty());
}

fn mixed_tree() -> TreeData {
    build_tree(&[
        user_message("user-1", None, "list files"),
        model_change("model-1", Some("user-1")),
        tool_call_only_assistant("asst-1", Some("model-1")),
        tool_result("result-1", Some("asst-1"), "tc-asst-1", "read"),
        assistant_message("asst-2", Some("result-1"), "done"),
    ])
}

#[test]
fn filters_toggle_and_cycle_with_status_labels() {
    let (selected, cancelled) = (Calls::new(), Calls::new());
    let harness = mount(mixed_tree(), "asst-2", &selected, &cancelled);
    let text = harness.screen();
    assert!(text.contains("[read: test.ts]"), "{text}");
    assert!(!text.contains("[model: claude-sonnet-4]"));
    assert_eq!(status(&harness), "  (3/3)");

    harness.send(&keys::ctrl('t'));
    assert_eq!(status(&harness), "  (2/2) [no-tools]");
    assert!(!harness.screen().contains("[read: test.ts]"));
    harness.send(&keys::ctrl('t'));
    assert_eq!(status(&harness), "  (3/3)");

    harness.send(&keys::ctrl('a'));
    assert!(harness.screen().contains("[model: claude-sonnet-4]"));
    assert_eq!(status(&harness), "  (4/4) [all]", "tool-call-only assistant messages stay hidden");

    harness.send(&keys::ctrl('d'));
    for expected in ["  (2/2) [no-tools]", "  (1/1) [user]", "  (0/0) [labeled]", "  (1/4) [all]", "  (1/3)"] {
        harness.send(&keys::ctrl('o'));
        assert_eq!(status(&harness), expected);
    }
    harness.send(SHIFT_CTRL_O);
    assert_eq!(status(&harness), "  (1/4) [all]");
    harness.send(SHIFT_CTRL_O);
    assert!(harness.screen().contains("  No entries found"));
    assert_eq!(status(&harness), "  (0/0) [labeled]");
    harness.send(&keys::ctrl('u'));
    assert_eq!(row(&harness, "list files"), "› • user: list files");
    assert_eq!(status(&harness), "  (1/1) [user]");
}

#[test]
fn folding_shows_markers_on_connectors_and_roots() {
    let (selected, cancelled) = (Calls::new(), Calls::new());
    let harness = mount(branching_tree(), "asst-4a", &selected, &cancelled);
    harness.send_all(&[keys::CTRL_LEFT, keys::CTRL_LEFT]);
    assert_eq!(row(&harness, "branch A start"), "› ├⊞ • user: branch A start");
    assert!(!harness.screen().contains("branch A leaf"));
    harness.send_all(&[keys::CTRL_LEFT, keys::CTRL_LEFT]);
    assert_eq!(row(&harness, "first message"), "› ⊞ • user: first message");
    assert!(harness.screen().contains("  (1/1)"));
    harness.send(keys::CTRL_RIGHT);
    assert_eq!(row(&harness, "branch A start"), "  ├⊞ • user: branch A start");
}

#[test]
fn edits_labels_inline_and_reports_changes() {
    let changes: Calls<(String, Option<String>)> = Calls::new();
    let record = changes.clone();
    let harness = Harness::new(100, 40, move |_| {
        let on_label_change =
            move |id: &str, label: Option<&str>| record.push((id.to_string(), label.map(String::from)));
        TreeSelector::new(
            branching_tree(),
            Some("asst-4a"),
            24,
            |_: &str| {},
            || {},
            Some(Box::new(on_label_change)),
            None,
            None,
        )
    });
    harness.send(&keys::shift('l'));
    let text = harness.screen();
    assert!(text.contains("  Label (empty to remove):"), "{text}");
    assert!(text.contains("  enter save  escape/ctrl+c cancel"), "{text}");
    assert!(!text.contains("branch A leaf"), "the tree is hidden while editing");
    harness.type_text(" milestone ");
    harness.send(keys::ENTER);
    assert_eq!(changes.take(), [("asst-4a".to_string(), Some("milestone".to_string()))]);
    assert_eq!(row(&harness, "branch A leaf"), "› │     • [milestone] assistant: branch A leaf");

    harness.send("L");
    assert!(harness.screen().contains("milestone"), "the editor starts with the current label");
    harness.send(keys::ESCAPE);
    assert!(changes.is_empty(), "escape closes the editor without a change");
    assert!(harness.screen().contains("branch A leaf"));

    harness.send("L");
    harness.send(keys::END);
    for _ in 0.."milestone".len() {
        harness.send(keys::BACKSPACE);
    }
    harness.send(keys::ENTER);
    assert_eq!(changes.take(), [("asst-4a".to_string(), None)]);
    assert_eq!(row(&harness, "branch A leaf"), "› │     • assistant: branch A leaf");
}

#[test]
fn toggles_label_timestamps_from_the_terminal() {
    let mut tree = branching_tree();
    tree.nodes[0].label = Some("start".into());
    tree.nodes[0].label_timestamp = Some(crate::session::iso_timestamp());
    let (selected, cancelled) = (Calls::new(), Calls::new());
    let harness = mount(tree, "asst-4a", &selected, &cancelled);
    assert_eq!(row(&harness, "first message"), "  • [start] user: first message");
    harness.send(&keys::shift('t'));
    let line = row(&harness, "first message");
    assert!(line.starts_with("  • [start] ") && line.ends_with(" user: first message"), "{line}");
    let time = line.trim_start_matches("  • [start] ").trim_end_matches(" user: first message");
    assert!(time.len() == 5 && time.as_bytes()[2] == b':', "today shows HH:MM: {time}");
    assert!(harness.screen().contains("(8/11) [+label time]"));
}

#[test]
fn copy_reports_none_for_entries_without_text() {
    let copied: Calls<Option<String>> = Calls::new();
    let record = copied.clone();
    let tree = build_tree(&[user_message("user-1", None, "hello"), model_change("model-1", Some("user-1"))]);
    let harness = Harness::new(100, 40, move |_| {
        let mut selector =
            TreeSelector::new(tree, Some("model-1"), 24, |_: &str| {}, || {}, None, None, Some(FilterMode::All));
        selector.set_on_copy(move |text| record.push(text.map(String::from)));
        selector
    });
    harness.send(&keys::ctrl('x'));
    harness.send(keys::UP);
    harness.send(&keys::ctrl('x'));
    assert_eq!(copied.get(), [None, Some("hello".to_string())]);
}

#[test]
fn an_empty_tree_cancels_after_a_short_delay() {
    let cancelled = Calls::new();
    let on_cancel = cancelled.counter();
    let harness = Harness::new(80, 20, move |_| {
        TreeSelector::new(TreeData::default(), None, 24, |_: &str| {}, on_cancel, None, None, None)
    });
    assert!(harness.screen().contains("  No entries found"));
    assert!(harness.screen().contains("  (0/0)"));
    assert!(cancelled.is_empty());
    harness.run_for(Duration::from_millis(150));
    assert_eq!(cancelled.len(), 1);
}

#[test]
fn deep_rows_scroll_horizontally_and_keep_the_cursor_gutter() {
    let mut entries = vec![user_message("u0", None, "root")];
    let mut parent = "u0".to_string();
    for depth in 1..=12 {
        let (main, side) = (format!("u{depth}"), format!("s{depth}"));
        entries.push(user_message(&main, Some(&parent), &format!("deep {depth}")));
        entries.push(user_message(&side, Some(&parent), &format!("side {depth}")));
        parent = main;
    }
    let tree = build_tree(&entries);
    let harness =
        Harness::new(40, 60, move |_| TreeSelector::new(tree, Some("u12"), 60, |_: &str| {}, || {}, None, None, None));
    // The prefix of `deep 12` is 36 columns wide, so its text is visible only when scrolled.
    let line = row(&harness, "deep 12");
    assert!(line.starts_with("› "), "{line}");
    assert!(line.contains("• user: deep 12"), "{line}");
    assert_eq!(status(&harness), "  (13/25)");
    for line in harness.viewport() {
        assert!(pi_tui::utils::visible_width(&line) <= 40, "{line}");
    }
}
