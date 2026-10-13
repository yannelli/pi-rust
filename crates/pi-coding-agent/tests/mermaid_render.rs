//! Snapshot parity with upstream `grok-mermaid` 0.2.3. Each fixture under
//! `tests/fixtures/mermaid/` was rendered by the TypeScript package; the Rust port
//! must reproduce `plain`, `styled`, `width`, `warnings` and null-ness exactly.

use std::borrow::Cow;
use std::path::PathBuf;

use pi_coding_agent::mermaid::{AnsiTheme, DEFAULT_THEME, MermaidArt, diagram_kind, render, source_box, to_ansi};
use serde_json::Value;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mermaid")
}

fn load(file: &str) -> Value {
    let path = fixture_dir().join(file);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// All cases from `<stem>_1.json`, `<stem>_2.json`, ... (split to keep files short).
fn load_cases(stem: &str) -> Vec<Value> {
    let mut cases = Vec::new();
    for n in 1.. {
        let file = format!("{stem}_{n}.json");
        if !fixture_dir().join(&file).exists() {
            break;
        }
        match load(&file) {
            Value::Array(chunk) => cases.extend(chunk),
            _ => panic!("{file} is not an array"),
        }
    }
    assert!(!cases.is_empty(), "no fixtures for {stem}");
    cases
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_else(|| panic!("fixture field `{key}` is not a string"))
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().expect("array").iter().map(|v| v.as_str().expect("string").to_string()).collect()
}

/// Compare one rendered art against its fixture, describing the first mismatch.
fn compare_art(actual: Option<&MermaidArt>, expected: &Value) -> Result<(), String> {
    let (actual, expected) = match (actual, expected.is_null()) {
        (None, true) => return Ok(()),
        (Some(_), true) => return Err("expected null, got art".into()),
        (None, false) => return Err("expected art, got null".into()),
        (Some(actual), false) => (actual, expected),
    };
    let plain = strings(&expected["plain"]);
    if actual.plain != plain {
        return Err(format!("plain differs\nexpected:\n{}\nactual:\n{}", plain.join("\n"), actual.plain.join("\n")));
    }
    let styled: Vec<Vec<(String, String)>> = expected["styled"]
        .as_array()
        .expect("styled rows")
        .iter()
        .map(|row| {
            let spans = row.as_array().expect("styled row");
            spans.iter().map(|s| (s[0].as_str().unwrap().to_string(), s[1].as_str().unwrap().to_string())).collect()
        })
        .collect();
    let actual_styled: Vec<Vec<(String, String)>> = actual
        .styled
        .iter()
        .map(|row| row.iter().map(|s| (s.text.clone(), s.cls.as_str().to_string())).collect())
        .collect();
    if actual_styled != styled {
        let row = actual_styled.iter().zip(&styled).position(|(a, e)| a != e).unwrap_or(styled.len());
        return Err(format!(
            "styled differs at row {row}\nexpected: {:?}\nactual:   {:?}",
            styled.get(row),
            actual_styled.get(row)
        ));
    }
    let width = expected["width"].as_u64().expect("width") as usize;
    if actual.width != width {
        return Err(format!("width: expected {width}, got {}", actual.width));
    }
    let warnings = strings(&expected["warnings"]);
    if actual.warnings != warnings {
        return Err(format!("warnings: expected {warnings:?}, got {:?}", actual.warnings));
    }
    Ok(())
}

/// Render every case of a fixture group and report all mismatches at once.
fn check_render_fixture(stem: &str) {
    let cases = load_cases(stem);
    let mut failures = Vec::new();
    for case in &cases {
        let name = str_of(case, "name");
        let src = str_of(case, "src");
        let kind = diagram_kind(src).map(|k| k.as_str());
        if kind != case["kind"].as_str() {
            failures.push(format!("{name}: diagram_kind expected {:?}, got {kind:?}", case["kind"]));
        }
        if let Err(err) = compare_art(render(src).as_ref(), &case["art"]) {
            failures.push(format!("{name}: {err}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{stem}: {} of {} cases differ\n\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n\n")
    );
}

#[test]
fn flowchart_fixtures_match_upstream() {
    check_render_fixture("flowchart");
}

#[test]
fn state_fixtures_match_upstream() {
    check_render_fixture("state");
}

#[test]
fn class_fixtures_match_upstream() {
    check_render_fixture("class");
}

#[test]
fn er_fixtures_match_upstream() {
    check_render_fixture("er");
}

#[test]
fn sequence_fixtures_match_upstream() {
    check_render_fixture("sequence");
}

#[test]
fn misc_fixtures_match_upstream() {
    check_render_fixture("misc");
}

#[test]
fn size_limit_fixtures_match_upstream() {
    check_render_fixture("limits");
}

#[test]
fn source_box_fixtures_match_upstream() {
    let mut failures = Vec::new();
    for case in &load_cases("source_box") {
        let max_width = case["maxWidth"].as_u64().map(|w| w as usize);
        let art = source_box(str_of(case, "src"), max_width);
        if let Err(err) = compare_art(Some(&art), &case["art"]) {
            failures.push(format!("{}: {err}", str_of(case, "name")));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

fn theme_from(value: &Value) -> AnsiTheme {
    let sgr = |key: &str| value[key].as_str().map(|s| Cow::Owned(s.to_string()));
    AnsiTheme {
        border: sgr("border"),
        text: sgr("text"),
        edge: sgr("edge"),
        edge_label: sgr("edgeLabel"),
        title: sgr("title"),
        none: sgr("none"),
    }
}

#[test]
fn ansi_fixtures_match_upstream() {
    let fixture = load("ansi.json");
    assert_eq!(theme_from(&fixture["defaultTheme"]), DEFAULT_THEME);
    let custom = theme_from(&fixture["customTheme"]);
    for case in fixture["cases"].as_array().expect("cases") {
        let name = str_of(case, "name");
        let src = str_of(case, "src");
        let (art, theme) = match str_of(case, "mode") {
            "sourceBox" => (source_box(src, None), &DEFAULT_THEME),
            "custom" => (render(src).expect("renders"), &custom),
            _ => (render(src).expect("renders"), &DEFAULT_THEME),
        };
        assert_eq!(to_ansi(&art, theme), strings(&case["lines"]), "{name}");
    }
}
