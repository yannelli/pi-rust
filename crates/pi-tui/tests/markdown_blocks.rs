//! Port of upstream `markdown.test.ts`: block spacing, block quotes, and streaming code fences.

mod markdown_common;

use markdown_common::{
    CYAN, Capabilities, MAGENTA, YELLOW, chalk, default_theme, markdown, plain, strip_ansi, styled_markdown,
};
use pi_tui::Component;
use pi_tui::components::{DefaultTextStyle, Markdown, MarkdownOptions};

fn rendered_plain(text: &str, width: usize) -> Vec<String> {
    plain(&markdown(text).render(width))
}

/// Number of empty lines after the first line matching `found`.
fn blank_lines_after(lines: &[String], found: impl Fn(&str) -> bool) -> Option<usize> {
    let index = lines.iter().position(|line| found(line)).expect("anchor line");
    lines[index + 1..].iter().position(|line| !line.is_empty())
}

#[test]
fn keeps_one_blank_line_after_code_blocks() {
    let _caps = Capabilities::plain();
    let lines = rendered_plain("hello world\n\n```js\nconst hello = \"world\";\n```\n\nagain, hello world", 80);
    assert_eq!(blank_lines_after(&lines, |line| line == "```"), Some(1), "{lines:?}");

    let expected = ["hello this is text", "", "```", "  code block", "```", "", "more text"];
    for text in [
        "hello this is text\n```\ncode block\n```\nmore text",
        "hello this is text\n\n```\ncode block\n```\n\nmore text",
    ] {
        assert_eq!(rendered_plain(text, 80), expected, "{text:?}");
    }
}

#[test]
fn does_not_add_a_trailing_blank_line_after_the_last_block() {
    let _caps = Capabilities::plain();
    for text in [
        "```js\nconst hello = 'world';\n```",
        "hello world\n\n```js\nconst hello = 'world';\n```",
        "---",
        "# Hello",
        "> This is a quote",
    ] {
        let lines = rendered_plain(text, 80);
        assert_ne!(lines.last().map(String::as_str), Some(""), "{text:?}: {lines:?}");
    }
}

#[test]
fn keeps_one_blank_line_after_dividers_headings_and_quotes() {
    let _caps = Capabilities::plain();
    let lines = rendered_plain("hello world\n\n---\n\nagain, hello world", 80);
    assert_eq!(blank_lines_after(&lines, |line| line.contains('─')), Some(1));
    let lines = rendered_plain("# Hello\n\nThis is a paragraph", 80);
    assert_eq!(blank_lines_after(&lines, |line| line.contains("Hello")), Some(1));
    let lines = rendered_plain("hello world\n\n> This is a quote\n\nagain, hello world", 80);
    assert_eq!(blank_lines_after(&lines, |line| line.contains("This is a quote")), Some(1));
}

fn quoted_lines(lines: &[String]) -> Vec<String> {
    lines.iter().map(|line| strip_ansi(line)).filter(|line| line.starts_with("│ ")).collect()
}

#[test]
fn styles_all_lines_of_lazy_and_explicit_multiline_quotes() {
    let _caps = Capabilities::plain();
    for (source, color, code) in [(">Foo\nbar", &[MAGENTA], "\x1b[35m"), (">Foo\n>bar", &[CYAN], "\x1b[36m")] {
        let style = DefaultTextStyle { color: Some(chalk(color)), ..DefaultTextStyle::default() };
        let lines = styled_markdown(source, 0, style).render(80);
        assert_eq!(quoted_lines(&lines).len(), 2, "{lines:?}");
        for needle in ["Foo", "bar"] {
            let line = lines.iter().find(|line| line.contains(needle)).expect("line");
            assert!(line.contains("\x1b[3m"), "{needle} italic: {line:?}");
            // Block quotes use the quote theme and skip the default message color.
            assert!(!line.contains(code), "{needle} colored: {line:?}");
        }
    }
}

#[test]
fn renders_list_content_inside_blockquotes() {
    let _caps = Capabilities::plain();
    let quoted = quoted_lines(&markdown("> 1. bla bla\n> - nested bullet").render(80));
    assert!(quoted.iter().any(|line| line.contains("1. bla bla")), "{quoted:?}");
    assert!(quoted.iter().any(|line| line.contains("- nested bullet")), "{quoted:?}");
}

#[test]
fn wraps_long_blockquote_lines_with_a_border_on_each_line() {
    let _caps = Capabilities::plain();
    let text = "This is a very long blockquote line that should wrap to multiple lines when rendered";
    let lines: Vec<String> =
        rendered_plain(&format!("> {text}"), 30).into_iter().filter(|line| !line.is_empty()).collect();
    assert!(lines.len() > 1);
    assert!(lines.iter().all(|line| line.starts_with("│ ")), "{lines:?}");
    let all = lines.join(" ");
    for needle in ["very long", "blockquote", "multiple"] {
        assert!(all.contains(needle));
    }

    let style = DefaultTextStyle { color: Some(chalk(&[YELLOW])), italic: true, ..DefaultTextStyle::default() };
    let raw = styled_markdown("> This is styled text that is long enough to wrap", 0, style).render(25);
    let lines: Vec<String> = plain(&raw).into_iter().filter(|line| !line.is_empty()).collect();
    assert!(lines.iter().all(|line| line.starts_with("│ ")), "{lines:?}");
    let output = raw.join("\n");
    assert!(output.contains("\x1b[3m") && !output.contains("\x1b[33m"), "{output:?}");
}

// Regression test for https://github.com/earendil-works/pi/issues/11272
#[test]
fn renders_quotes_nested_deeper_than_the_width_allows_without_exploding() {
    let _caps = Capabilities::plain();
    let mut markdown =
        Markdown::new(&format!("{}x", ">".repeat(60)), 1, 0, default_theme(), None, MarkdownOptions::default());
    let lines: Vec<String> = plain(&markdown.render(80)).iter().map(|line| line.trim_end().to_string()).collect();
    assert_eq!(lines.len(), 1, "Expected a single line: {lines:?}");
    assert!(lines[0].ends_with('x'), "Expected content to be kept: {lines:?}");
}

#[test]
fn renders_inline_formatting_inside_blockquotes_and_reapplies_quote_styling() {
    let _caps = Capabilities::plain();
    let lines = markdown("> Quote with **bold** and `code`").render(80);
    assert!(!quoted_lines(&lines).is_empty());
    let all = plain(&lines).join(" ");
    for needle in ["Quote with", "bold", "code"] {
        assert!(all.contains(needle));
    }
    let output = lines.join("\n");
    assert!(output.contains("\x1b[1m") && output.contains("\x1b[33m") && output.contains("\x1b[3m"), "{output:?}");
}

#[test]
fn stabilizes_partial_closing_fence_rendering() {
    let _caps = Capabilities::plain();
    let cases: [(&str, &[&str]); 6] = [
        ("```ts\nconst x = 1;\n``", &["```ts", "  const x = 1;", "```"]),
        ("```md\nnot a closing fence:\n``\n```", &["```md", "  not a closing fence:", "  ``", "```"]),
        ("```ts\n``", &["```ts", "", "```"]),
        ("````\n```", &["```", "", "```"]),
        ("~~~~~\n~~~~", &["```", "", "```"]),
        (
            "```md\nnot a closing fence:\n``\n```\n\nafter",
            &["```md", "  not a closing fence:", "  ``", "```", "", "after"],
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(rendered_plain(input, 80), expected, "{input:?}");
    }
    let partial = markdown("```ts\nconst x = 1;\n``").render(80);
    let complete = markdown("```ts\nconst x = 1;\n```").render(80);
    assert_eq!(partial.len(), complete.len());
}
