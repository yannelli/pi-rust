//! The `/fork` user message selector.

use std::time::Duration;

use pi_tui::Component;

use crate::interactive_components::test_support::{Calls, Harness, keys, render_plain, setup};
use crate::interactive_components::user_message_selector::{UserMessageItem, UserMessageList, UserMessageSelector};

fn messages(count: usize) -> Vec<UserMessageItem> {
    (1..=count).map(|n| UserMessageItem::new(&format!("e{n}"), &format!("message {n}"))).collect()
}

fn selected_line(lines: &[String]) -> String {
    lines.iter().find(|line| line.starts_with("› ")).cloned().unwrap_or_default()
}

#[test]
fn renders_header_and_selects_the_newest_message() {
    let harness = Harness::new(120, 30, |_| UserMessageSelector::new(messages(3), |_| {}, || {}, None));
    let screen = harness.screen();
    assert!(screen.contains("Fork from Message"), "{screen}");
    assert!(screen.contains("Select a user message to copy the active path up to that point into a new session"));
    assert!(screen.contains("Message 1 of 3") && screen.contains("Message 3 of 3"));
    assert_eq!(selected_line(&harness.viewport()), "› message 3");
    assert!(!screen.contains("(3/3)"), "no scroll indicator for short lists");
}

#[test]
fn navigation_wraps_and_enter_reports_the_entry_id() {
    let selected = Calls::new();
    let harness = Harness::new(80, 30, |_| UserMessageSelector::new(messages(3), selected.recorder(), || {}, None));
    harness.send(keys::DOWN);
    assert_eq!(selected_line(&harness.viewport()), "› message 1");
    harness.send(keys::UP);
    assert_eq!(selected_line(&harness.viewport()), "› message 3");
    harness.send_all(&[keys::UP, keys::UP]);
    assert_eq!(selected_line(&harness.viewport()), "› message 1");
    harness.send(keys::ENTER);
    assert_eq!(selected.get(), ["e1"]);
}

#[test]
fn initial_selection_and_escape_cancel() {
    let cancelled = Calls::new();
    let harness =
        Harness::new(80, 30, |_| UserMessageSelector::new(messages(3), |_| {}, cancelled.counter(), Some("e2")));
    assert_eq!(selected_line(&harness.viewport()), "› message 2");
    harness.send(keys::ESCAPE);
    assert_eq!(cancelled.len(), 1);
}

#[test]
fn unknown_initial_id_falls_back_to_the_newest_message() {
    let _kb = setup();
    let list = UserMessageList::new(messages(4), Some("missing"));
    assert_eq!(list.selected_index(), 3);
}

#[test]
fn long_lists_scroll_and_show_position() {
    let _kb = setup();
    let mut selector = UserMessageSelector::new(messages(15), |_| {}, || {}, None);
    let lines = render_plain(&mut selector, 80);
    assert!(lines.contains(&"  (15/15)".to_string()), "{lines:#?}");
    assert!(lines.iter().any(|line| line.contains("Message 6 of 15")));
    assert!(!lines.iter().any(|line| line.contains("Message 5 of 15")));
    selector.handle_input(keys::DOWN);
    let lines = render_plain(&mut selector, 80);
    assert!(lines.contains(&"  (1/15)".to_string()));
    assert_eq!(selected_line(&lines), "› message 1");
}

#[test]
fn messages_are_flattened_and_truncated() {
    let _kb = setup();
    let items = vec![UserMessageItem::new("a", "  first line\nsecond line that is long  ")];
    let mut selector = UserMessageSelector::new(items, |_| {}, || {}, None);
    let lines = render_plain(&mut selector, 24);
    assert_eq!(selected_line(&lines), "› first line second l...");
}

#[test]
fn empty_list_shows_message_and_cancels_after_a_delay() {
    let cancelled = Calls::new();
    let harness = Harness::new(80, 20, |_| UserMessageSelector::new(Vec::new(), |_| {}, cancelled.counter(), None));
    assert!(harness.screen().contains("No user messages found"));
    assert!(cancelled.is_empty());
    harness.send(keys::ENTER);
    assert!(cancelled.is_empty());
    harness.run_for(Duration::from_millis(150));
    assert_eq!(cancelled.len(), 1);
}
