mod session;
mod share;

use serde_json::{Value, json};

use super::ansi::{ansi_lines_to_html, ansi_to_html, escape_html};
use super::html::{js_replace_first, strip_sgr};
use super::json::{js_number, js_to_fixed, stringify};
use super::theme::{ExportTheme, derive_export_colors};
use super::tool_renderer::{AnsiToolRenderer, AnsiToolRendererAdapter, ToolHtmlRenderer, pre_render_custom_tools};

#[test]
fn numbers_print_like_javascript() {
    let cases = [
        (1.0, "1"),
        (-2.5, "-2.5"),
        (0.1, "0.1"),
        (1e-6, "0.000001"),
        (1e-7, "1e-7"),
        (1.5e-7, "1.5e-7"),
        (1e21, "1e+21"),
        (1e20, "100000000000000000000"),
        (123_456_789_012_345_680_000_000.0, "1.2345678901234569e+23"),
        (-0.0, "0"),
        (0.000_123, "0.000123"),
    ];
    for (value, expected) in cases {
        assert_eq!(js_number(value), expected, "{value:e}");
    }
}

#[test]
fn to_fixed_rounds_ties_away_from_zero() {
    assert_eq!(js_to_fixed(1.125, 2), "1.13");
    assert_eq!(js_to_fixed(1.005, 2), "1.00");
    assert_eq!(js_to_fixed(1.2, 2), "1.20");
    assert_eq!(js_to_fixed(9.999, 2), "10.00");
    assert_eq!(js_to_fixed(-0.001, 2), "-0.00");
    assert_eq!(js_to_fixed(2.5, 0), "3");
}

#[test]
fn stringify_orders_keys_and_escapes_like_javascript() {
    let value: Value =
        serde_json::from_str(r#"{"b":1,"10":2,"a":[1.0,{"2":0,"1":0,"01":0}],"s":"\u0007\"\n</script>"}"#).unwrap();
    assert_eq!(stringify(&value), r#"{"10":2,"b":1,"a":[1,{"1":0,"2":0,"01":0}],"s":"\u0007\"\n</script>"}"#);
    let big: Value = serde_json::from_str("[12345678901234567890, 9007199254740993]").unwrap();
    assert_eq!(stringify(&big), "[12345678901234567000,9007199254740992]");
}

#[test]
fn replace_expands_dollar_patterns_like_javascript() {
    assert_eq!(js_replace_first("a{{X}}b{{X}}", "{{X}}", "1"), "a1b{{X}}");
    assert_eq!(js_replace_first("a{{X}}b", "{{X}}", "$$-$&-$`-$'-$1-$<n>-$"), "a$-{{X}}-a-b-$1-$<n>-$b");
    assert_eq!(js_replace_first("abc", "zz", "$&"), "abc");
}

#[test]
fn ansi_converts_styles_to_spans() {
    assert_eq!(ansi_to_html("plain <b>"), "plain &lt;b&gt;");
    assert_eq!(
        ansi_to_html("\x1b[1;31mred\x1b[0m done"),
        "<span style=\"color:#800000;font-weight:bold\">red</span> done"
    );
    assert_eq!(
        ansi_to_html("\x1b[38;5;196mx\x1b[48;2;1;2;3my\x1b[39;49m"),
        "<span style=\"color:#ff0000\">x</span><span style=\"color:#ff0000;background-color:rgb(1,2,3)\">y</span>"
    );
    assert_eq!(ansi_to_html("\x1b[38;5;240mg\x1b[m"), "<span style=\"color:#585858\">g</span>");
    assert_eq!(
        ansi_to_html("\x1b[2;3;4mz\x1b[22;23;24m"),
        "<span style=\"opacity:0.6;font-style:italic;text-decoration:underline\">z</span>"
    );
    assert_eq!(
        ansi_lines_to_html(&["a", "", "\x1b[0m"]),
        "<div class=\"ansi-line\">a</div><div class=\"ansi-line\">&nbsp;</div><div class=\"ansi-line\">&nbsp;</div>"
    );
    assert_eq!(escape_html("&<>\"'"), "&amp;&lt;&gt;&quot;&#039;");
    assert_eq!(strip_sgr("\x1b[1mbold\x1b[0m \x1b[x"), "bold \x1b[x");
}

#[test]
fn theme_backgrounds_prefer_export_colors_then_derive() {
    let dark = ExportTheme::dark();
    assert_eq!(dark.backgrounds(), ["#21252c".to_string(), "#282c34".into(), "#4e2f1b".into()]);
    let custom = ExportTheme::new([("text", "#ffffff"), ("userMessageBg", "#343541")]);
    assert_eq!(
        custom.backgrounds(),
        ["rgb(36, 37, 46)".to_string(), "rgb(44, 45, 55)".into(), "rgb(72, 68, 65)".into()]
    );
    assert_eq!(
        custom.css_vars(),
        "--text: #ffffff;\n      --userMessageBg: #343541;\n      --exportPageBg: rgb(36, 37, 46);\n      --exportCardBg: rgb(44, 45, 55);\n      --exportInfoBg: rgb(72, 68, 65);"
    );
    assert_eq!(
        derive_export_colors("rgb(250, 250, 250)"),
        ["rgb(240, 240, 240)".to_string(), "rgb(250, 250, 250)".into(), "rgb(255, 255, 230)".into()]
    );
    assert_eq!(derive_export_colors("okhsl(1 2% 3%)")[0], "rgb(24, 24, 30)");
    assert_eq!(ExportTheme::builtin("light").unwrap().backgrounds()[0], ExportTheme::light().backgrounds()[0]);
    assert!(ExportTheme::builtin("nope").is_none());
}

struct Lines;

impl AnsiToolRenderer for Lines {
    fn render_call_lines(&self, _: &str, name: &str, _: &Value, width: usize) -> Option<Vec<String>> {
        (name == "custom").then(|| vec![format!("\x1b[1m{name}\x1b[0m {width}")])
    }

    fn render_result_lines(
        &self,
        _: &str,
        _: &str,
        content: &[Value],
        _: &Value,
        _: bool,
        expanded: bool,
        _: usize,
    ) -> Option<Vec<String>> {
        let mut lines = vec![String::new(), "\x1b[0m ".into(), format!("{} blocks", content.len())];
        if expanded {
            lines.push("more".into());
        }
        lines.push(String::new());
        Some(lines)
    }
}

#[test]
fn ansi_renderer_adapter_trims_and_collapses() {
    let adapter = AnsiToolRendererAdapter::new(Lines);
    assert_eq!(
        adapter.render_call("1", "custom", &json!({})).unwrap(),
        "<div class=\"ansi-line\"><span style=\"font-weight:bold\">custom</span> 100</div>"
    );
    assert!(adapter.render_call("1", "other", &json!({})).is_none());
    let result = adapter.render_result("1", "custom", &[json!({})], &Value::Null, false).unwrap();
    assert_eq!(result.collapsed.as_deref(), Some("<div class=\"ansi-line\">1 blocks</div>"));
    assert_eq!(
        result.expanded.as_deref(),
        Some("<div class=\"ansi-line\">1 blocks</div><div class=\"ansi-line\">more</div>")
    );
}

#[test]
fn pre_render_skips_template_tools() {
    let entries = vec![
        json!({"type": "message", "id": "1", "message": {"role": "assistant", "content": [
            {"type": "toolCall", "id": "c1", "name": "bash", "arguments": {}},
            {"type": "toolCall", "id": "c2", "name": "custom", "arguments": {}},
        ]}}),
        json!({"type": "message", "id": "2", "message": {"role": "toolResult", "toolCallId": "c1", "toolName": "bash", "content": []}}),
        json!({"type": "message", "id": "3", "message": {"role": "toolResult", "toolCallId": "c2", "toolName": "custom", "content": []}}),
    ];
    let rendered = pre_render_custom_tools(&entries, &AnsiToolRendererAdapter::new(Lines));
    assert_eq!(rendered.len(), 1);
    let (id, record) = &rendered[0];
    assert_eq!(id, "c2");
    assert!(record.call_html.is_some() && record.result_html_expanded.is_some());
    let json = serde_json::to_value(record).unwrap();
    let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["callHtml", "resultHtmlCollapsed", "resultHtmlExpanded"]);
}
