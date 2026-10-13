//! Row text of each entry type, tool call summaries, search text, labels, session trees, and
//! the branch summary prompt flow.

use std::path::Path;

use pi_ai::Message;
use pi_tui::Component;
use serde_json::json;

use super::*;
use crate::agent_session::NavigateTreeOptions;
use crate::interactive_components::test_support::{keys, plain, setup};
use crate::interactive_components::tree_selector::{
    BRANCH_SUMMARY_PROMPT_TITLE, BranchSummaryChoice, BranchSummaryFlow, BranchSummaryStep, CUSTOM_INSTRUCTIONS_TITLE,
    FilterMode,
};
use crate::session::SessionManager;

/// The `all` view of `entries` (leaf is the last entry), one row per entry, cursor stripped.
fn rows(entries: &[Entry]) -> Vec<String> {
    let tree = build_tree(entries);
    let leaf = entries.last().map(|entry| entry.id().to_string()).unwrap_or_default();
    let mut selector =
        TreeSelector::new(tree, Some(&leaf), 100, |_: &str| {}, || {}, None, None, Some(FilterMode::All));
    let lines = plain(&selector.tree_list_mut().render(200));
    let body = |line: &String| line.chars().skip(2).collect::<String>().trim_start_matches("• ").to_string();
    lines[..lines.len() - 1].iter().map(body).collect()
}

#[test]
fn renders_each_entry_type() {
    let _kb = setup();
    let entries = [
        user_message("u", None, "line one\n\tline two  "),
        with(
            "message",
            "bash",
            Some("u"),
            json!({"message": {"role": "bashExecution", "command": "ls\n-la", "output": "", "timestamp": 1}}),
        ),
        with(
            "message",
            "sys",
            Some("bash"),
            json!({"message": {"role": "branchSummary", "summary": "s", "timestamp": 1}}),
        ),
        with(
            "custom_message",
            "cm",
            Some("sys"),
            json!({"customType": "note", "content": [{"type": "text", "text": "a"}, {"type": "image"}, {"type": "text", "text": "b"}]}),
        ),
        with("compaction", "comp", Some("cm"), json!({"summary": "x", "firstKeptEntryId": "u", "tokensBefore": 12500})),
        with("branch_summary", "bs", Some("comp"), json!({"fromId": "u", "summary": "went\nelsewhere"})),
        with("thinking_level_change", "think", Some("bs"), json!({"thinkingLevel": "high"})),
        with("custom", "custom", Some("think"), json!({"customType": "ext-state"})),
        with("context_edit", "edit", Some("custom"), json!({"targetId": "u", "replacement": {"content": "new"}})),
        with("label", "label", Some("edit"), json!({"targetId": "u", "label": null})),
        with("session_info", "title-empty", Some("label"), json!({})),
        with("session_info", "title", Some("title-empty"), json!({"name": "My session"})),
        assistant("aborted", Some("title"), json!([]), "aborted"),
        with(
            "message",
            "err",
            Some("aborted"),
            json!({"message": {"role": "assistant", "content": [], "stopReason": "error", "errorMessage": format!("boom\n{}", "x".repeat(100)), "timestamp": 1}}),
        ),
        assistant("empty", Some("err"), json!([{"type": "thinking", "thinking": "hmm"}]), "stop"),
    ];
    assert_eq!(
        rows(&entries),
        [
            "user: line one  line two",
            "[bash]: ls -la",
            "[branchSummary]",
            "[note]: ab",
            "[compaction: 13k tokens]",
            "[branch summary]: went elsewhere",
            "[thinking: high]",
            "[custom: ext-state]",
            "[context replace: u]",
            "[label: (cleared)]",
            "[title: empty]",
            "[title: My session]",
            "assistant: (aborted)",
            format!("assistant: boom {}", "x".repeat(75)).as_str(),
            "assistant: (no content)",
        ]
    );
}

#[test]
fn summarizes_tool_calls_by_tool() {
    let _kb = setup();
    let home = std::env::var("HOME").unwrap_or_default();
    let long = format!(r#"[custom: {{"long":"{}...]"#, "z".repeat(31));
    let calls = [
        (
            "read",
            json!({"path": format!("{home}/src/main.rs"), "offset": 5, "limit": 10}),
            "[read: ~/src/main.rs:5-14]",
        ),
        ("read", json!({"file_path": "a.rs", "limit": 3}), "[read: a.rs:1-3]"),
        ("read", json!({"path": "b.rs", "offset": 7}), "[read: b.rs:7]"),
        ("write", json!({"path": "w.txt"}), "[write: w.txt]"),
        ("edit", json!({"file_path": "e.txt"}), "[edit: e.txt]"),
        ("bash", json!({"command": "echo\thi"}), "[bash: echo hi]"),
        ("bash", json!({"command": "y".repeat(60)}), "[bash: yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy...]"),
        ("grep", json!({"pattern": "fn main"}), "[grep: /fn main/ in .]"),
        ("find", json!({"pattern": "*.rs", "path": "src"}), "[find: *.rs in src]"),
        ("ls", json!({}), "[ls: .]"),
        ("custom", json!({"key": "value"}), r#"[custom: {"key":"value"}]"#),
        ("custom", json!({"long": "z".repeat(40)}), long.as_str()),
    ];
    let mut entries = vec![user_message("u", None, "go")];
    let mut parent = "u".to_string();
    for (index, (name, args, _)) in calls.iter().enumerate() {
        let (call, result) = (format!("call-{index}"), format!("result-{index}"));
        let content = json!([{"type": "toolCall", "id": format!("tc-{index}"), "name": name, "arguments": args}]);
        entries.push(assistant(&call, Some(&parent), content, "toolUse"));
        entries.push(tool_result(&result, Some(&call), &format!("tc-{index}"), name));
        parent = result;
    }
    entries.push(tool_result("orphan", Some(&parent), "missing", "mystery"));
    entries.push(assistant_message("done", Some("orphan"), "ok"));
    let tree = build_tree(&entries);
    let mut selector = TreeSelector::new(tree, Some("done"), 100, |_: &str| {}, || {}, None, None, None);
    let text = plain(&selector.tree_list_mut().render(200)).join("\n");
    for (_, _, expected) in &calls {
        assert!(text.contains(expected), "missing {expected}:\n{text}");
    }
    assert!(text.contains("[mystery]"), "{text}");
}

#[test]
fn search_matches_labels_types_and_all_tokens() {
    let _kb = setup();
    let mut tree = build_tree(&[
        user_message("u1", None, "Alpha request"),
        assistant_message("a1", Some("u1"), "beta answer"),
        model_change("m1", Some("a1")),
        user_message("u2", Some("m1"), "gamma"),
    ]);
    tree.nodes[1].label = Some("Milestone".into());
    let mut selector = selector(&tree, "u2");
    let ids = |selector: &TreeSelector| selector.tree_list().visible_ids().join(",");
    for ch in "milestone".chars() {
        selector.handle_input(&ch.to_string());
    }
    assert_eq!(ids(&selector), "a1");
    selector.handle_input(keys::ESCAPE);
    for ch in "request USER".chars() {
        selector.handle_input(&ch.to_string());
    }
    assert_eq!(selector.tree_list().get_search_query(), "request USER");
    assert_eq!(ids(&selector), "u1");
    selector.handle_input(keys::ESCAPE);
    selector.handle_input(&keys::ctrl('a'));
    for ch in "model claude".chars() {
        selector.handle_input(&ch.to_string());
    }
    assert_eq!(ids(&selector), "m1");
    selector.handle_input("\x1b[1;5A");
    assert_eq!(selector.tree_list().get_search_query(), "model claude", "control sequences are not typed");
}

#[test]
fn multiple_roots_render_without_virtual_connectors() {
    let _kb = setup();
    let tree = build_tree(&[
        user_message("r1", None, "first"),
        user_message("r1a", Some("r1"), "first a"),
        user_message("r1b", Some("r1"), "first b"),
        user_message("r2", None, "second"),
    ]);
    let mut selector = selector(&tree, "r1a");
    let lines = plain(&selector.tree_list_mut().render(80));
    assert_eq!(lines, ["  • user: first", "› ├─ • user: first a", "  └─ user: first b", "  user: second", "  (2/4)",]);
}

#[test]
fn builds_from_a_session_tree_with_labels() {
    let _kb = setup();
    let mut session = SessionManager::in_memory(Path::new("/tmp"));
    let first = session.append_message(&Message::user("hello")).unwrap();
    session.append_label_change(&first, Some("start")).unwrap();
    session.append_message(&Message::user("again")).unwrap();
    let leaf = session.leaf_id().map(String::from);
    let tree = TreeData::from(&session.get_tree());
    let mut selector = TreeSelector::new(tree, leaf.as_deref(), 24, |_: &str| {}, || {}, None, None, None);
    let text = plain(&selector.render(80)).join("\n");
    assert!(text.contains("• [start] user: hello"), "{text}");
    assert!(text.contains("› • user: again"), "{text}");
    assert!(!text.contains("[label: start]"), "label entries are hidden by default");
}

#[test]
fn updates_labels_with_timestamps() {
    let _kb = setup();
    let tree = build_tree(&[user_message("u", None, "hello")]);
    let mut selector = selector(&tree, "u");
    let list = selector.tree_list_mut();
    list.update_node_label("u", Some("kept"), Some("2020-01-02T03:04:05.000Z"));
    let node = list.get_selected_node().unwrap();
    assert_eq!(
        (node.label.as_deref(), node.label_timestamp.as_deref()),
        (Some("kept"), Some("2020-01-02T03:04:05.000Z"))
    );
    list.update_node_label("u", Some("fresh"), None);
    assert!(list.get_selected_node().unwrap().label_timestamp.as_deref().is_some_and(|stamp| stamp.ends_with('Z')));
    list.update_node_label("u", None, None);
    let node = list.get_selected_node().unwrap();
    assert_eq!((node.label.as_deref(), node.label_timestamp.as_deref()), (None, None));
}

#[test]
fn label_timestamps_from_other_years_show_the_year() {
    let _kb = setup();
    let mut tree = build_tree(&[user_message("u", None, "hello")]);
    let zone = jiff::tz::TimeZone::system();
    let old = jiff::civil::date(2019, 7, 4).at(9, 5, 0, 0).to_zoned(zone).unwrap();
    tree.nodes[0].label = Some("old".into());
    tree.nodes[0].label_timestamp = Some(crate::session::format_iso(old.timestamp().as_millisecond()));
    let mut selector = selector(&tree, "u");
    selector.handle_input(&keys::shift('t'));
    let text = plain(&selector.tree_list_mut().render(80)).join("\n");
    assert!(text.contains("[old] 19/7/4 09:05 user: hello"), "{text}");
}

#[test]
fn branch_summary_flow_follows_the_upstream_prompt() {
    assert_eq!(BRANCH_SUMMARY_PROMPT_TITLE, "Summarize branch?");
    assert_eq!(CUSTOM_INSTRUCTIONS_TITLE, "Custom summarization instructions");
    assert_eq!(BranchSummaryChoice::options(), ["No summary", "Summarize", "Summarize with custom prompt"]);
    assert_eq!(BranchSummaryChoice::from_label("Summarize"), Some(BranchSummaryChoice::Summarize));

    let (_, step) = BranchSummaryFlow::start("leaf", Some("leaf"), false);
    assert_eq!(step, BranchSummaryStep::AlreadyAtTarget);
    let navigate = |summarize: bool, custom_instructions: Option<&str>| BranchSummaryStep::Navigate {
        entry_id: "target".into(),
        options: NavigateTreeOptions {
            summarize,
            custom_instructions: custom_instructions.map(String::from),
            ..Default::default()
        },
    };
    let (_, step) = BranchSummaryFlow::start("target", Some("leaf"), true);
    assert_eq!(step, navigate(false, None), "skipPrompt navigates without a summary");

    let (mut flow, step) = BranchSummaryFlow::start("target", Some("leaf"), false);
    assert_eq!(step, BranchSummaryStep::ChooseSummary);
    assert_eq!(flow.choose(None), BranchSummaryStep::ReopenTree { entry_id: "target".into() });
    assert_eq!(flow.choose(Some("No summary")), navigate(false, None));
    assert_eq!(flow.choose(Some("Summarize")), navigate(true, None));
    assert_eq!(flow.choose(Some("Summarize with custom prompt")), BranchSummaryStep::EditInstructions);
    assert_eq!(flow.custom_instructions(None), BranchSummaryStep::ChooseSummary, "cancel returns to the choice");
    assert_eq!(flow.choose(Some("Summarize with custom prompt")), BranchSummaryStep::EditInstructions);
    assert_eq!(flow.custom_instructions(Some("focus on tests".into())), navigate(true, Some("focus on tests")));
    assert_eq!(flow.entry_id(), "target");
}

#[test]
fn starts_at_the_initial_selection_when_reopened() {
    let _kb = setup();
    let tree = branching_tree();
    let reopened =
        TreeSelector::new(tree.clone(), Some("asst-4a"), 24, |_: &str| {}, || {}, None, Some("user-3b"), None);
    assert_eq!(selected_id(&reopened).as_deref(), Some("user-3b"));
    let mut hidden = TreeSelector::new(tree, Some("asst-4a"), 24, |_: &str| {}, || {}, None, Some("asst-3b"), None);
    hidden.handle_input(&keys::ctrl('u'));
    assert_eq!(selected_id(&hidden).as_deref(), Some("user-3b"), "a hidden selection moves to its ancestor");
}
