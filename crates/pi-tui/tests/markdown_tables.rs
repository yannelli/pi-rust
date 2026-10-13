//! Port of upstream `markdown.test.ts`: tables.

mod markdown_common;

use std::rc::Rc;

use markdown_common::{
    Capabilities, Screen, column_from, column_of, default_theme, last_column_of, markdown, strip_ansi,
};
use pi_tui::Component;
use pi_tui::components::{Markdown, MarkdownOptions, MarkdownTheme};

fn plain_lines(lines: &[String]) -> Vec<String> {
    lines.iter().map(|line| strip_ansi(line).trim_end().to_string()).collect()
}

fn assert_width(lines: &[String], width: usize) {
    for line in lines {
        assert!(line.chars().count() <= width, "Line exceeds width {width}: {line:?}");
    }
}

const PEOPLE: &str = "| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |";

#[test]
fn renders_a_simple_table_with_row_dividers() {
    let _caps = Capabilities::plain();
    let lines = plain_lines(&markdown(PEOPLE).render(80));
    for needle in ["Name", "Age", "Alice", "Bob", "│", "─"] {
        assert!(lines.iter().any(|line| line.contains(needle)), "{needle} in {lines:?}");
    }
    assert_eq!(lines.iter().filter(|line| line.contains('┼')).count(), 2, "header and row divider");
}

#[test]
fn keeps_column_width_at_least_the_longest_word() {
    let _caps = Capabilities::plain();
    let source = "| Column One | Column Two |\n| --- | --- |\n| superlongword short | otherword |\n| small | tiny |";
    let lines = plain_lines(&markdown(source).render(32));
    let data = lines.iter().find(|line| line.contains("superlongword")).expect("data row");
    let segments: Vec<&str> = data.split('│').collect();
    let first_width = segments[1].chars().count() - 2;
    assert!(first_width >= "superlongword".len(), "{data}");
}

#[test]
fn renders_aligned_tables_and_varying_column_widths() {
    let _caps = Capabilities::plain();
    let lines = plain_lines(
        &markdown("| Left | Center | Right |\n| :--- | :---: | ---: |\n| A | B | C |\n| Long text | Middle | End |")
            .render(80),
    );
    for needle in ["Left", "Center", "Right", "Long text"] {
        assert!(lines.iter().any(|line| line.contains(needle)));
    }
    let lines = plain_lines(
        &markdown("| Short | Very long column header |\n| --- | --- |\n| A | This is a much longer cell content |\n| B | Short |")
            .render(80),
    );
    assert!(lines.iter().any(|line| line.contains("Very long column header")));
    assert!(lines.iter().any(|line| line.contains("This is a much longer cell content")));
}

#[test]
fn wraps_table_cells_when_the_table_exceeds_the_width() {
    let _caps = Capabilities::plain();
    let source = "| Command | Description | Example |\n| --- | --- | --- |\n| npm install | Install all dependencies | npm install |\n| npm run build | Build the project | npm run build |";
    let lines = plain_lines(&markdown(source).render(50));
    assert_width(&lines, 50);
    let all = lines.join(" ");
    for needle in ["Command", "Description", "npm install", "Install"] {
        assert!(all.contains(needle), "{needle}");
    }

    let lines =
        plain_lines(&markdown("| Header |\n| --- |\n| This is a very long cell content that should wrap |").render(25));
    let data_rows = lines.iter().filter(|line| line.starts_with('│') && !line.contains('─')).count();
    assert!(data_rows > 2, "{lines:?}");
    let all = lines.join(" ");
    for needle in ["very long", "cell content", "should wrap"] {
        assert!(all.contains(needle), "{needle}");
    }
}

#[test]
fn does_not_leak_wrapped_link_styles_into_table_borders_or_plain_cells() {
    let source =
        "| Link | Plain |\n| --- | --- |\n| [**one two three four five six**](https://example.com) | normal text |";
    for hyperlinks in [true, false] {
        let _caps = Capabilities::set(hyperlinks, false);
        let screen = Screen::show(24, 16, Rc::new(std::cell::RefCell::new(markdown(source))));
        let viewport = screen.viewport();
        let row = viewport.iter().position(|line| line.contains("one") && line.contains("norm")).expect("row");
        let line = &viewport[row];
        let link = column_of(line, "one").expect("link");
        let separator = column_from(line, "│", link).expect("separator");
        let plain = column_of(line, "norm").expect("plain");
        assert!(separator > link && plain > separator);
        assert!(!screen.fg_default(row, link));
        assert!(screen.fg_default(row, separator));
        assert!(screen.fg_default(row, plain));
        assert!(screen.cell(row, link, |cell| cell.bold()));
        assert!(!screen.cell(row, separator, |cell| cell.bold()));
        assert!(!screen.cell(row, plain, |cell| cell.bold()));

        if !hyperlinks {
            let url_row = viewport.iter().position(|line| line.contains("https")).expect("fallback url row");
            let url_line = &viewport[url_row];
            let url = column_of(url_line, "https").expect("url");
            let url_separator = column_from(url_line, "│", url).expect("separator");
            let border = last_column_of(url_line, "│").expect("border");
            assert!(url_separator > url && border > url_separator);
            assert!(screen.cell(url_row, url, |cell| cell.dim()));
            assert!(!screen.cell(url_row, url_separator, |cell| cell.dim()));
            assert!(!screen.cell(url_row, border, |cell| cell.dim()));
        }
    }
}

#[test]
fn restores_the_enclosing_style_after_a_wrapped_table_link() {
    let _caps = Capabilities::set(true, true);
    let theme = MarkdownTheme {
        // A basic wrapper that does not reopen itself after nested resets.
        quote: Rc::new(|text| format!("\x1b[38;2;18;52;86m{text}\x1b[39m")),
        link: Rc::new(|text| format!("\x1b[38;2;129;162;190m{text}\x1b[39m")),
        ..default_theme()
    };
    let source =
        "> | Link | Plain |\n> | --- | --- |\n> | [one two three four five six](https://example.com) | normal text |";
    let md = Markdown::new(source, 0, 0, theme, None, MarkdownOptions::default());
    let screen = Screen::show(28, 10, Rc::new(std::cell::RefCell::new(md)));
    let quote_color = vt100::Color::Rgb(0x12, 0x34, 0x56);
    let viewport = screen.viewport();
    let row = viewport.iter().position(|line| line.contains("one") && line.contains("normal")).expect("row");
    let line = &viewport[row];
    let link = column_of(line, "one").expect("link");
    let separator = column_from(line, "│", link).expect("separator");
    let plain = column_of(line, "normal").expect("plain");
    assert!(separator > link && plain > separator);
    assert_ne!(screen.fg(row, link), quote_color);
    assert_eq!(screen.fg(row, separator), quote_color);
    assert_eq!(screen.fg(row, plain), quote_color);

    let final_row = viewport.iter().position(|line| line.contains("five six")).expect("final row");
    let final_line = &viewport[final_row];
    let final_link = column_of(final_line, "five six").expect("link");
    let final_separator = column_from(final_line, "│", final_link).expect("separator");
    let final_border = last_column_of(final_line, "│").expect("border");
    assert!(final_separator > final_link && final_border > final_separator);
    assert_eq!(screen.fg(final_row, final_separator), quote_color);
    assert_eq!(screen.fg(final_row, final_border), quote_color);
}

#[test]
fn wraps_long_unbroken_tokens_and_styled_code_inside_cells() {
    let _caps = Capabilities::plain();
    let url = "https://example.com/this/is/a/very/long/url/that/should/wrap";
    let lines = plain_lines(&markdown(&format!("| Value |\n| --- |\n| prefix {url} |")).render(30));
    assert_width(&lines, 30);
    let table: Vec<&String> = lines.iter().filter(|line| line.starts_with('│')).collect();
    assert!(!table.is_empty());
    for line in table {
        assert_eq!(line.matches('│').count(), 2, "{line}");
    }
    let extracted: String =
        lines.join("").chars().filter(|c| !matches!(c, '│' | '├' | '┤' | '─') && !c.is_whitespace()).collect();
    assert!(extracted.contains("prefix") && extracted.contains(url));

    let raw = markdown("| Code |\n| --- |\n| `averyveryveryverylongidentifier` |").render(20);
    assert!(raw.join("\n").contains("\x1b[33m"), "inline code is yellow");
    let lines = plain_lines(&raw);
    assert_width(&lines, 20);
    for line in lines.iter().filter(|line| line.starts_with('│')) {
        assert_eq!(line.matches('│').count(), 2, "{line}");
    }
}

#[test]
fn handles_narrow_and_natural_widths() {
    let _caps = Capabilities::plain();
    let lines = plain_lines(&markdown("| A | B | C |\n| --- | --- | --- |\n| 1 | 2 | 3 |").render(15));
    assert!(!lines.is_empty());
    assert_width(&lines, 15);

    let lines = plain_lines(&markdown("| A | B |\n| --- | --- |\n| 1 | 2 |").render(80));
    assert!(lines.iter().find(|line| line.contains('A') && line.contains('B')).is_some_and(|line| line.contains('│')));
    assert!(lines.iter().any(|line| line.contains('├') && line.contains('┼')));
    assert!(lines.iter().any(|line| line.contains('1') && line.contains('2')));
}

#[test]
fn respects_padding_and_ends_without_a_trailing_blank_line() {
    let _caps = Capabilities::plain();
    let source = "| Column One | Column Two |\n| --- | --- |\n| Data 1 | Data 2 |";
    let mut md = Markdown::new(source, 2, 0, default_theme(), None, MarkdownOptions::default());
    let lines = plain_lines(&md.render(40));
    assert_width(&lines, 40);
    assert!(lines.iter().find(|line| line.contains('│')).is_some_and(|line| line.starts_with("  ")));

    let lines = plain_lines(&markdown("| Name |\n| --- |\n| Alice |").render(80));
    assert_ne!(lines.last().map(String::as_str), Some(""), "{lines:?}");
}
