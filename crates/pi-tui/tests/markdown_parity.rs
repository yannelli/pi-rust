//! Rendering parity with upstream: `fixtures/markdown_marked_parity.json` holds Markdown (from
//! the upstream docs, the upstream tests, and random structural input) with the lines the
//! TypeScript `Markdown` component renders for it, using the simple theme below.

mod markdown_common;

use std::rc::Rc;

use markdown_common::Capabilities;
use pi_tui::Component;
use pi_tui::components::{Markdown, MarkdownOptions, MarkdownTheme, StyleFn};
use serde_json::Value;

fn wrap(open: &'static str, close: &'static str) -> StyleFn {
    Rc::new(move |text: &str| format!("\x1b[{open}m{text}\x1b[{close}m"))
}

fn theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: wrap("36", "39"),
        link: wrap("34", "39"),
        link_url: wrap("2", "22"),
        code: wrap("33", "39"),
        code_block: wrap("32", "39"),
        code_block_border: wrap("2", "22"),
        quote: wrap("3", "23"),
        quote_border: wrap("2", "22"),
        hr: wrap("2", "22"),
        list_bullet: wrap("36", "39"),
        bold: wrap("1", "22"),
        italic: wrap("3", "23"),
        strikethrough: wrap("9", "29"),
        underline: wrap("4", "24"),
        highlight_code: None,
        code_block_indent: None,
    }
}

#[test]
fn renders_like_the_upstream_component() {
    let _caps = Capabilities::set(false, false);
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/markdown_marked_parity.json")).expect("valid fixture");
    assert!(cases.len() > 300);
    let mut failures = Vec::new();
    for (index, case) in cases.iter().enumerate() {
        let options = &case["options"];
        let options = MarkdownOptions {
            preserve_ordered_list_markers: options["preserveOrderedListMarkers"].as_bool().unwrap_or(false),
            preserve_backslash_escapes: options["preserveBackslashEscapes"].as_bool().unwrap_or(false),
            transform: None,
            render_latex: options["renderLatex"].as_bool().unwrap_or(true),
        };
        let text = case["text"].as_str().expect("text");
        let padding_x = case["paddingX"].as_u64().expect("padding") as usize;
        let width = case["width"].as_u64().expect("width") as usize;
        let expected: Vec<String> = serde_json::from_value(case["expected"].clone()).expect("expected lines");
        let actual = Markdown::new(text, padding_x, 0, theme(), None, options).render(width);
        if actual != expected {
            failures.push(format!(
                "case {index} {text:?} at width {width}\n  expected {expected:?}\n  actual   {actual:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{} of {} cases differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}
