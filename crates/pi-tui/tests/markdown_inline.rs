//! Port of upstream `markdown.test.ts`: LaTeX math, escapes, pre-styled text, headings,
//! strikethrough, links, and HTML-like tags.

mod markdown_common;

use std::cell::RefCell;
use std::rc::Rc;

use markdown_common::{Capabilities, GRAY, Screen, chalk, default_theme, markdown, plain, strip_ansi, styled_markdown};
use pi_tui::components::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme};
use pi_tui::{Component, ComponentRef};

fn rendered_plain(text: &str) -> Vec<String> {
    plain(&markdown(text).render(80))
}

fn gray_italic() -> DefaultTextStyle {
    DefaultTextStyle { color: Some(chalk(&[GRAY])), italic: true, ..DefaultTextStyle::default() }
}

#[test]
fn renders_inline_dollar_and_parenthesis_delimiters() {
    let _caps = Capabilities::plain();
    let source = r"A map $\mathbb{C}^3 \to \mathbb{C}^3$, $xy$, $x-y$, $-x$, $\frac{1}{2}$, $\rightarrow$, and \(s \to \infty\).";
    assert_eq!(rendered_plain(source), ["A map ℂ³ → ℂ³, xy, x-y, -x, 1/2, →, and s → ∞."]);
}

#[test]
fn renders_display_delimiters() {
    let _caps = Capabilities::plain();
    let source = "Before\n\n$$\\{3x+2y,\\; x \\in \\{0, \\pm 1\\}\\}$$\n\nafter";
    assert_eq!(rendered_plain(source), ["Before", "", "{3x+2y, x ∈ {0, ± 1}}", "", "after"]);

    let source = "Before\n\n\\[\nE \\approx \\frac{0.1\\ \\text{lux}}{100\\ \\text{lm/W}}\n\\]\n\nafter";
    assert_eq!(rendered_plain(source), ["Before", "", "    0.1 lux", "E ≈ ────────", "    100 lm/W", "", "after"]);
}

#[test]
fn aligns_matrices_and_renders_lower_limits_in_display_math() {
    let _caps = Capabilities::plain();
    let source =
        "Consider the matrix\n\n\\[\nA=\n\\begin{pmatrix}\n\\pi & 0\\\\\n0 & \\frac{1}{\\pi}\n\\end{pmatrix}.\n\\]";
    assert_eq!(rendered_plain(source), ["Consider the matrix", "", "A = ⎛ π │ 0   ⎞", "    ⎝ 0 │ 1/π ⎠."]);

    let source = "\\[\n\\lim_{x\\to 0}\\frac{\\frac{\\sin x}{x}-1}{\\frac{e^x-1}{x}-1}=0\n\\]";
    assert_eq!(rendered_plain(source), ["     (sin x)/x-1", "lim  ─────────── = 0", "x→0  (eˣ-1)/x-1"]);
}

#[test]
fn renders_math_inside_lists_and_tables() {
    let _caps = Capabilities::plain();
    let output = rendered_plain("- Formula: $F_1 = u^2$\n\n| Value |\n| --- |\n| $\\mathbb{C}^3$ |").join("\n");
    assert!(output.contains("- Formula: F₁ = u²"), "{output}");
    assert!(output.contains("│ ℂ³"), "{output}");
}

#[test]
fn does_not_treat_currency_shell_variables_or_code_spans_as_math() {
    let _caps = Capabilities::plain();
    let source = "Costs $5 and $10 or $8k–$12k; use `$x$`, $HOME, and ${PATH}.";
    assert_eq!(rendered_plain(source), ["Costs $5 and $10 or $8k–$12k; use $x$, $HOME, and ${PATH}."]);
    let shell = "Paths: $HOME/$USER and $XDG_CONFIG_HOME/$APP_CONFIG";
    assert_eq!(rendered_plain(shell), [shell]);
}

#[test]
fn preserves_unsupported_incomplete_and_escaped_latex() {
    let _caps = Capabilities::plain();
    for source in [r"Unknown $x + \unknown{y}$ after", r"Streaming $\mathbb{C}^3"] {
        assert_eq!(rendered_plain(source), [source]);
    }
    assert_eq!(rendered_plain(r"Map \(\mathbb{C}^3"), [r"Map \(\mathbb{C}^3"]);
    assert_eq!(rendered_plain("\\[\nx^2"), ["\\[", "x^2"]);
    let source = "Escaped \\$x-y\\$.\n\n```text\n$\\mathbb{C}^3$\n```";
    assert_eq!(rendered_plain(source), ["Escaped $x-y$.", "", "```text", "  $\\mathbb{C}^3$", "```"]);
}

#[test]
fn allows_latex_rendering_to_be_disabled() {
    let _caps = Capabilities::plain();
    let source = "$$\n\\widetilde Y_{sf}\n=\n(1-w_{sf})\\mu_{sf}^{\\mathrm{MAR}}\n$$\n\nInline \\(A_{sf}\\)";
    let options = MarkdownOptions { render_latex: false, ..MarkdownOptions::default() };
    let mut md = Markdown::new(source, 0, 0, default_theme(), None, options);
    assert_eq!(
        plain(&md.render(80)),
        ["$$", r"\widetilde Y_{sf}", "=", r"(1-w_{sf})\mu_{sf}^{\mathrm{MAR}}", "$$", "", r"Inline \(A_{sf}\)"]
    );
}

const TABLE: [&str; 3] = ["| A | B |", "|---|---|", "| 1 | 2 |"];
const RENDERED_TABLE: [&str; 5] = ["┌───┬───┐", "│ A │ B │", "├───┼───┤", "│ 1 │ 2 │", "└───┴───┘"];

/// Joins `before`, the table lines, and `after` with newlines.
fn with_table(before: &[&str], after: &[&str]) -> String {
    [before, &TABLE, after].concat().join("\n")
}

/// The lines `before`, the rendered table, and `after`.
fn rendered_table(before: &[&str], after: &[&str]) -> Vec<String> {
    [before, &RENDERED_TABLE, after].concat().into_iter().map(String::from).collect()
}

#[test]
fn renders_a_table_after_an_orphan_dollar_opener() {
    let _caps = Capabilities::plain();
    let source = ["Intro", "", "$$", "", TABLE[0], TABLE[1], TABLE[2], "", "After"].join("\n");
    assert_eq!(rendered_plain(&source), rendered_table(&["Intro", "", "$$", ""], &["", "After"]));
    let source = with_table(&["$$", "x = 1", ""], &[]);
    assert_eq!(rendered_plain(&source), rendered_table(&["$$", "x = 1", ""], &[]));
}

#[test]
fn does_not_pair_an_unclosed_dollar_opener_with_a_later_one_across_a_table() {
    let _caps = Capabilities::plain();
    let source = with_table(&["Intro", "", "$$", "x = 1", ""], &["", "$$", "y^2", "$$"]);
    assert_eq!(rendered_plain(&source), rendered_table(&["Intro", "", "$$", "x = 1", ""], &["", "y²"]));
}

#[test]
fn renders_tables_with_pipe_free_delimiters_or_headers_after_an_orphan_dollar_opener() {
    let _caps = Capabilities::plain();
    let source = ["$$", "", "| A |", ":---:", "| 1 |"].join("\n");
    assert_eq!(rendered_plain(&source), ["$$", "", "┌───┐", "│ A │", "├───┤", "│ 1 │", "└───┘"]);
    let source = ["$$", "x^2", "", "Value", "| --- |", "1"].join("\n");
    let expected = ["$$", "x^2", "", "┌───────┐", "│ Value │", "├───────┤", "│ 1     │", "└───────┘"];
    assert_eq!(rendered_plain(&source), expected);
}

#[test]
fn renders_a_table_after_an_orphan_bracket_opener() {
    let _caps = Capabilities::plain();
    let source = with_table(&["\\\\[", "x^2", ""], &[]);
    assert_eq!(rendered_plain(&source), rendered_table(&["", "\\[", "x^2", ""], &[]));
}

#[test]
fn does_not_pair_an_unclosed_bracket_opener_with_a_later_one_across_a_table() {
    let _caps = Capabilities::plain();
    let source = with_table(&["\\\\[", "x = 1", ""], &["", "\\\\[", "y^2", "\\\\]"]);
    let expected = rendered_table(&["", "\\[", "x = 1", ""], &["", "\\", "", "\\[", "y^2", "\\\\]"]);
    assert_eq!(rendered_plain(&source), expected);
}

#[test]
fn still_renders_closed_multi_line_display_math_followed_by_a_table() {
    let _caps = Capabilities::plain();
    let source = with_table(&["$$", "a", "+ b", "$$", ""], &[]);
    assert_eq!(rendered_plain(&source), rendered_table(&["a + b", ""], &[]));
}

#[test]
fn switches_from_raw_to_rendered_math_when_a_streamed_delimiter_closes() {
    let _caps = Capabilities::plain();
    let mut md = markdown(r"Map $\mathbb{C}^3");
    assert_eq!(plain(&md.render(80)), [r"Map $\mathbb{C}^3"]);
    md.set_text(r"Map $\mathbb{C}^3$");
    assert_eq!(plain(&md.render(80)), ["Map ℂ³"]);
}

/// Renders with tagged bold and italic so emphasis boundaries show up in the text.
fn rendered_tagged(text: &str) -> Vec<String> {
    let theme = MarkdownTheme {
        bold: Rc::new(|text: &str| format!("<b>{text}</b>")),
        italic: Rc::new(|text: &str| format!("<i>{text}</i>")),
        ..default_theme()
    };
    let mut md = Markdown::new(text, 0, 0, theme, None, MarkdownOptions::default());
    md.render(80).iter().map(|line| line.trim_end().to_string()).collect()
}

#[test]
fn renders_emphasis_delimited_next_to_cjk_punctuation() {
    let _caps = Capabilities::plain();
    assert_eq!(rendered_tagged("**这是测试。**后面"), ["<b>这是测试。</b>后面"]);
    assert_eq!(rendered_tagged("**一句话说清：**一台机器"), ["<b>一句话说清：</b>一台机器"]);
    assert_eq!(rendered_tagged("**要点到此为止。**20 小时"), ["<b>要点到此为止。</b>20 小时"]);
    assert_eq!(rendered_tagged("字**「引用」**字"), ["字<b>「引用」</b>字"]);
    assert_eq!(rendered_tagged("*斜体。*后面"), ["<i>斜体。</i>后面"]);
}

#[test]
fn keeps_commonmark_flanking_rules_for_non_cjk_text() {
    let _caps = Capabilities::plain();
    assert_eq!(rendered_tagged("**end.**Then next"), ["**end.**Then next"]);
}

#[test]
fn normalizes_or_preserves_backslash_escapes() {
    let _caps = Capabilities::plain();
    assert_eq!(rendered_plain(r#""\""#), [r#""""#]);
    let options = MarkdownOptions { preserve_backslash_escapes: true, ..MarkdownOptions::default() };
    let mut md = Markdown::new(r#""\""#, 0, 0, default_theme(), None, options);
    assert_eq!(plain(&md.render(80)), [r#""\""#]);
}

#[test]
fn preserves_gray_italic_styling_after_inline_code_and_bold() {
    let _caps = Capabilities::plain();
    let output = styled_markdown("This is thinking with `inline code` and more text after", 1, gray_italic())
        .render(80)
        .join("\n");
    assert!(output.contains("inline code"));
    assert!(output.contains("\x1b[90m") && output.contains("\x1b[3m") && output.contains("\x1b[33m"), "{output:?}");

    let output =
        styled_markdown("This is thinking with **bold text** and more after", 1, gray_italic()).render(80).join("\n");
    assert!(output.contains("bold text"));
    assert!(output.contains("\x1b[90m") && output.contains("\x1b[3m") && output.contains("\x1b[1m"), "{output:?}");
}

/// Upstream `MarkdownWithInput`: the Markdown lines followed by an `INPUT` line.
struct MarkdownWithInput {
    markdown: Markdown,
    line_count: Rc<RefCell<usize>>,
}

impl Component for MarkdownWithInput {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = self.markdown.render(width);
        *self.line_count.borrow_mut() = lines.len();
        lines.push("INPUT".into());
        lines
    }

    fn invalidate(&mut self) {
        self.markdown.invalidate();
    }
}

#[test]
fn does_not_leak_styles_into_following_lines_when_rendered_in_a_tui() {
    let _caps = Capabilities::plain();
    let line_count = Rc::new(RefCell::new(0));
    let component: ComponentRef = Rc::new(RefCell::new(MarkdownWithInput {
        markdown: styled_markdown("This is thinking with `inline code`", 1, gray_italic()),
        line_count: line_count.clone(),
    }));
    let screen = Screen::show(80, 6, component);
    let input_row = *line_count.borrow();
    assert!(input_row > 0);
    assert!(!screen.cell(input_row, 0, |cell| cell.italic()));
}

/// The 40 bytes before `needle` in `output` (upstream `slice(index - 40, index)`).
fn preceding(output: &str, needle: &str) -> String {
    let index = output.find(needle).expect("needle");
    let chars: Vec<char> = output[..index].chars().collect();
    chars[chars.len().saturating_sub(40)..].iter().collect()
}

#[test]
fn preserves_heading_styling_after_inline_code_and_bold() {
    let _caps = Capabilities::plain();
    let output = markdown("### Why `sourceInfo` should not be optional").render(80).join("\n");
    assert!(output.contains("\x1b[33m"));
    let chunk = preceding(&output, "should not be optional");
    assert!(chunk.contains("\x1b[1m") && chunk.contains("\x1b[36m"), "{chunk:?}");

    let output = markdown("# Title with `code` inside").render(80).join("\n");
    let chunk = preceding(&output, "inside");
    assert!(chunk.contains("\x1b[1m") && chunk.contains("\x1b[36m") && chunk.contains("\x1b[4m"), "{chunk:?}");

    let output = markdown("## Heading with **bold** and more").render(80).join("\n");
    let chunk = preceding(&output, "and more");
    assert!(chunk.contains("\x1b[1m") && chunk.contains("\x1b[36m"), "{chunk:?}");
}

#[test]
fn does_not_leak_h1_underline_into_padding_when_inline_code_is_last() {
    let _caps = Capabilities::plain();
    let component = Rc::new(RefCell::new(markdown("# Important distinction from `open()`")));
    let screen = Screen::show(80, 4, component.clone());
    let line = component.borrow_mut().render(80)[0].clone();
    let content_width = strip_ansi(&line).trim_end().chars().count();
    assert!(content_width > 0);
    for col in content_width..80 {
        assert!(!screen.cell(0, col, |cell| cell.underline()), "underline in padding at col {col}");
    }
}

#[test]
fn renders_strikethrough_only_for_double_tildes() {
    let _caps = Capabilities::plain();
    let lines = markdown("Use ~~strikethrough~~ here").render(80);
    let joined_plain = plain(&lines).join(" ");
    assert!(lines.join("\n").contains("\x1b[9m"));
    assert!(joined_plain.contains("strikethrough") && !joined_plain.contains("~~strikethrough~~"));

    let lines = markdown("Use ~strikethrough~ literally").render(80);
    assert!(plain(&lines).join(" ").contains("~strikethrough~"));
    assert!(!lines.join("\n").contains("\x1b[9m"));
}

#[test]
fn renders_links_without_hyperlink_support() {
    let _caps = Capabilities::set(false, false);
    let joined = plain(&markdown("Contact user@example.com for help").render(80)).join(" ");
    assert!(joined.contains("user@example.com") && !joined.contains("mailto:"));
    let joined = plain(&markdown("Visit https://example.com for more").render(80)).join(" ");
    assert_eq!(joined.matches("https://example.com").count(), 1);
    let joined = plain(&markdown("[click here](https://example.com)").render(80)).join(" ");
    assert!(joined.contains("click here") && joined.contains("(https://example.com)"));
    let joined = plain(&markdown("[Email me](mailto:test@example.com)").render(80)).join(" ");
    assert!(joined.contains("Email me") && joined.contains("(mailto:test@example.com)"));
}

fn without_osc8_and_sgr(lines: &[String]) -> String {
    let osc = regex::Regex::new(r"\x1b\]8;;[^\x1b]*\x1b\\").expect("regex");
    lines.iter().map(|line| strip_ansi(&osc.replace_all(line, ""))).collect()
}

#[test]
fn emits_osc8_hyperlinks_when_the_terminal_supports_them() {
    let _caps = Capabilities::set(true, false);
    let lines = markdown("[click here](https://example.com)").render(80);
    let joined = lines.join("");
    assert!(joined.contains("\x1b]8;;https://example.com\x1b\\") && joined.contains("\x1b]8;;\x1b\\"));
    let visible = without_osc8_and_sgr(&lines);
    assert!(visible.contains("click here") && !visible.contains("(https://example.com)"));

    let joined = markdown("[Email me](mailto:test@example.com)").render(80).join("");
    assert!(joined.contains("\x1b]8;;mailto:test@example.com\x1b\\") && joined.contains("\x1b]8;;\x1b\\"));

    let lines = markdown("Visit https://example.com for more").render(80);
    assert!(lines.join("").contains("\x1b]8;;https://example.com\x1b\\"));
    assert!(!without_osc8_and_sgr(&lines).contains("(https://example.com)"));
}

#[test]
fn renders_html_like_tags_as_text() {
    let _caps = Capabilities::plain();
    let joined =
        plain(&markdown("This is text with <thinking>hidden content</thinking> that should be visible").render(80))
            .join(" ");
    assert!(joined.contains("hidden content") || joined.contains("<thinking>"));
    let joined = plain(&markdown("```html\n<div>Some HTML</div>\n```").render(80)).join("\n");
    assert!(joined.contains("<div>") && joined.contains("</div>"));
}
