//! Port of upstream `markdown.test.ts`: transforms, lists, and combined features.

mod markdown_common;

use std::cell::RefCell;
use std::rc::Rc;

use markdown_common::{Capabilities, default_theme, markdown, plain, strip_ansi};
use pi_tui::Component;
use pi_tui::components::{Markdown, MarkdownOptions};

fn rendered_plain(text: &str, width: usize) -> Vec<String> {
    plain(&markdown(text).render(width))
}

fn contains_line(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|line| line.contains(needle))
}

#[test]
fn caches_transformed_markdown_by_source_and_available_width() {
    let _caps = Capabilities::plain();
    let calls: Rc<RefCell<Vec<(String, usize)>>> = Rc::default();
    let sink = calls.clone();
    let options = MarkdownOptions {
        transform: Some(Rc::new(move |source: &str, width: usize| {
            sink.borrow_mut().push((source.to_string(), width));
            format!("{source} {width}")
        })),
        ..MarkdownOptions::default()
    };
    let mut md = Markdown::new("source", 2, 0, default_theme(), None, options);
    let trimmed = |lines: Vec<String>| lines.iter().map(|line| strip_ansi(line).trim().to_string()).collect::<Vec<_>>();

    assert_eq!(trimmed(md.render(80)), ["source 76"]);
    md.render(80);
    assert_eq!(trimmed(md.render(60)), ["source 56"]);
    assert_eq!(*calls.borrow(), [("source".to_string(), 76), ("source".to_string(), 56)]);

    md.set_text("updated");
    assert_eq!(trimmed(md.render(60)), ["updated 56"]);
    assert_eq!(calls.borrow().last(), Some(&("updated".to_string(), 56)));

    md.invalidate();
    md.render(60);
    assert_eq!(calls.borrow().last(), Some(&("updated".to_string(), 56)));
    assert_eq!(calls.borrow().len(), 4);
}

#[test]
fn renders_simple_and_deeply_nested_lists() {
    let _caps = Capabilities::plain();
    let lines = rendered_plain("- Item 1\n  - Nested 1.1\n  - Nested 1.2\n- Item 2", 80);
    for needle in ["- Item 1", "    - Nested 1.1", "    - Nested 1.2", "- Item 2"] {
        assert!(contains_line(&lines, needle), "{needle} in {lines:?}");
    }

    let lines = rendered_plain("- Level 1\n  - Level 2\n    - Level 3\n      - Level 4", 80);
    for needle in ["- Level 1", "    - Level 2", "        - Level 3", "            - Level 4"] {
        assert!(contains_line(&lines, needle), "{needle} in {lines:?}");
    }
}

#[test]
fn renders_ordered_and_mixed_nested_lists() {
    let _caps = Capabilities::plain();
    let lines = rendered_plain("1. First\n   1. Nested first\n   2. Nested second\n2. Second", 80);
    for needle in ["1. First", "    1. Nested first", "    2. Nested second", "2. Second"] {
        assert!(contains_line(&lines, needle), "{needle} in {lines:?}");
    }

    let lines = rendered_plain(
        "1. Ordered item\n   - Unordered nested\n   - Another nested\n2. Second ordered\n   - More nested",
        80,
    );
    for needle in ["1. Ordered item", "    - Unordered nested", "2. Second ordered"] {
        assert!(contains_line(&lines, needle), "{needle} in {lines:?}");
    }
}

#[test]
fn normalizes_ordered_list_markers_by_default() {
    let _caps = Capabilities::plain();
    assert_eq!(rendered_plain("1. alpha\n1. beta\n1. gamma", 80), ["1. alpha", "2. beta", "3. gamma"]);
}

#[test]
fn preserves_source_list_markers_when_configured() {
    let _caps = Capabilities::plain();
    let options = MarkdownOptions { preserve_ordered_list_markers: true, ..MarkdownOptions::default() };
    let source = "  4. forth\n  3. third\n\n10) ten\n7) seven\n\n+ plus\n* star\n- minus\n+";
    let mut md = Markdown::new(source, 0, 0, default_theme(), None, options);
    assert_eq!(
        plain(&md.render(80)),
        ["4. forth", "3. third", "", "10) ten", "7) seven", "", "+ plus", "* star", "- minus", "+"]
    );
}

#[test]
fn renders_blank_lines_between_loose_list_items() {
    let _caps = Capabilities::plain();
    let source = "1. Lorem ipsum dolor sit amet.\n\n   Ut enim ad minim veniam.\n\n2. Duis aute irure dolor.\n\n   Excepteur sint occaecat cupidatat.\n\n3. Beep boop";
    assert_eq!(
        rendered_plain(source, 80),
        [
            "1. Lorem ipsum dolor sit amet.",
            "",
            "   Ut enim ad minim veniam.",
            "",
            "2. Duis aute irure dolor.",
            "",
            "   Excepteur sint occaecat cupidatat.",
            "",
            "3. Beep boop",
        ]
    );
}

#[test]
fn renders_task_list_markers() {
    let _caps = Capabilities::plain();
    assert_eq!(rendered_plain("- [ ] beep\n- [x] boop", 80), ["- [ ] beep", "- [x] boop"]);
}

#[test]
fn maintains_numbering_when_code_blocks_are_not_indented() {
    let _caps = Capabilities::plain();
    let source = "1. First item\n\n```typescript\n// code block\n```\n\n2. Second item\n\n```typescript\n// another code block\n```\n\n3. Third item";
    let lines: Vec<String> = rendered_plain(source, 80).iter().map(|line| line.trim().to_string()).collect();
    let numbered: Vec<&String> = lines
        .iter()
        .filter(|line| {
            line.split_once('.').is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        })
        .collect();
    assert_eq!(numbered.len(), 3, "{numbered:?}");
    for (line, expected) in numbered.iter().zip(["1.", "2.", "3."]) {
        assert!(line.starts_with(expected), "{line}");
    }
}

#[test]
fn indents_wrapped_list_lines() {
    let _caps = Capabilities::plain();
    assert_eq!(rendered_plain("- alpha beta gamma delta epsilon", 20), ["- alpha beta gamma", "  delta epsilon"]);
    assert_eq!(rendered_plain("1. alpha beta gamma delta epsilon", 20), ["1. alpha beta gamma", "   delta epsilon"]);
    assert_eq!(rendered_plain("10. alpha beta gamma delta epsilon", 21), ["10. alpha beta gamma", "    delta epsilon"]);
    assert_eq!(
        rendered_plain("- parent\n  - alpha beta gamma delta epsilon", 24),
        ["- parent", "    - alpha beta gamma", "      delta epsilon"]
    );
    assert_eq!(
        rendered_plain("1. parent\n   - alpha beta gamma delta epsilon", 24),
        ["1. parent", "    - alpha beta gamma", "      delta epsilon"]
    );
}

#[test]
fn renders_and_wraps_blockquotes_and_code_blocks_inside_list_items() {
    let _caps = Capabilities::plain();
    assert_eq!(
        rendered_plain("- > alpha beta gamma delta epsilon zeta", 24),
        ["- │ alpha beta gamma", "  │ delta epsilon zeta"]
    );
    assert_eq!(
        rendered_plain("- ```ts\n  alpha beta gamma delta epsilon zeta\n  ```", 24),
        ["- ```ts", "    alpha beta gamma", "  delta epsilon zeta", "  ```"]
    );
}

#[test]
fn renders_lists_and_tables_together() {
    let _caps = Capabilities::plain();
    let source = "# Test Document\n\n- Item 1\n  - Nested item\n- Item 2\n\n| Col1 | Col2 |\n| --- | --- |\n| A | B |";
    let lines: Vec<String> = markdown(source).render(80).iter().map(|line| strip_ansi(line)).collect();
    for needle in ["Test Document", "- Item 1", "    - Nested item", "Col1", "│"] {
        assert!(contains_line(&lines, needle), "{needle} in {lines:?}");
    }
}
