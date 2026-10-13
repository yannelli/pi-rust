//! Port of upstream `latex.test.ts`. The cases live in `fixtures/latex_cases.json`, extracted
//! from the upstream test file: source, display mode, and expected output (`null` = unsupported).

use pi_tui::latex::{RenderLatexOptions, render_latex, render_latex_with};
use serde_json::Value;

#[test]
fn renders_the_upstream_latex_cases() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("fixtures/latex_cases.json")).expect("valid fixture");
    assert!(cases.len() > 100);
    let mut failures = Vec::new();
    for case in &cases {
        let source = case["source"].as_str().expect("source");
        let display = case["display"].as_bool().expect("display");
        let expected = case["expected"].as_str();
        let actual = render_latex_with(source, RenderLatexOptions { display });
        if actual.as_deref() != expected {
            failures.push(format!("{source:?} (display {display})\n  expected {expected:?}\n  actual   {actual:?}"));
        }
    }
    assert!(failures.is_empty(), "{} of {} cases failed:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

#[test]
fn renders_inline_math_with_the_default_options() {
    assert_eq!(render_latex(r"\mathbb{C}^3 \to \mathbb{C}^3").as_deref(), Some("ℂ³ → ℂ³"));
    assert_eq!(render_latex(r"x + \unknown{y}"), None);
}
