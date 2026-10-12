//! HTML export against `fixtures/export_session.jsonl`. The expected hashes come from
//! upstream `exportFromFile` (dark theme) and `exportSessionToHtml` (light theme, system
//! prompt, tools, and the mock renderer below) run on the same fixture.

use std::path::{Path, PathBuf};

use base64::Engine;
use pi_coding_agent::export::json::stringify;
use pi_coding_agent::export::{
    ExportTheme, ExportTool, HtmlExportOptions, RenderedResultHtml, ToolHtmlRenderer, default_html_output_path,
    export_from_file, export_session_html,
};
use serde_json::{Value, json};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/export_session.jsonl")
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3))
}

fn session_data(html: &str) -> Value {
    let marker = "<script id=\"session-data\" type=\"application/json\">";
    let start = html.find(marker).unwrap() + marker.len();
    let end = start + html[start..].find("</script>").unwrap();
    let json = base64::engine::general_purpose::STANDARD.decode(&html[start..end]).unwrap();
    serde_json::from_slice(&json).unwrap()
}

struct MockRenderer;

impl ToolHtmlRenderer for MockRenderer {
    fn render_call(&self, id: &str, name: &str, args: &Value) -> Option<String> {
        Some(format!("<span class=\"call\">{name}:{id}:{}</span>", stringify(args)))
    }

    fn render_result(
        &self,
        _: &str,
        name: &str,
        content: &[Value],
        _: &Value,
        error: bool,
    ) -> Option<RenderedResultHtml> {
        Some(RenderedResultHtml {
            collapsed: Some(format!("<i>{name} collapsed {error}</i>")),
            expanded: Some(format!("<i>{name} expanded {}</i>", content.len())),
        })
    }
}

#[test]
fn export_from_file_matches_upstream_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.html");
    let written = export_from_file(fixture().to_str().unwrap(), out.to_str(), &ExportTheme::dark()).unwrap();
    assert_eq!(written, out);
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(bytes.len(), 276_772);
    assert_eq!(fnv1a(&bytes), 0xb20e_7b57_27d8_9d24);
}

#[test]
fn session_export_with_state_matches_upstream_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("state.html");
    let options = HtmlExportOptions {
        theme: ExportTheme::light(),
        system_prompt: Some("You are pi. Use </script> carefully & $& stays.".into()),
        tools: Some(vec![
            ExportTool {
                name: "bash".into(),
                description: Some("Run a command".into()),
                parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
            },
            ExportTool {
                name: "codemode".into(),
                description: None,
                parameters: json!({"type": "object", "properties": {}}),
            },
        ]),
        tool_renderer: Some(&MockRenderer),
    };
    export_session_html(&fixture(), out.to_str(), &options).unwrap();
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(bytes.len(), 277_476);
    assert_eq!(fnv1a(&bytes), 0x4d67_2059_beeb_2670);

    let data = session_data(&String::from_utf8(bytes).unwrap());
    let rendered = &data["renderedTools"];
    assert_eq!(rendered.as_object().unwrap().keys().collect::<Vec<_>>(), ["call_code"]);
    assert_eq!(rendered["call_code"]["resultHtmlExpanded"], "<i>codemode expanded 1</i>");
    assert!(data["tools"][1].get("description").is_none());
}

#[test]
fn page_structure_embedded_data_and_escaping() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("page.html");
    export_from_file(fixture().to_str().unwrap(), out.to_str(), &ExportTheme::dark()).unwrap();
    let html = std::fs::read_to_string(&out).unwrap();

    assert!(html.starts_with("<!DOCTYPE html>\n<html lang=\"en\">"));
    assert!(html.contains("<title>Session Export</title>"));
    assert!(html.contains("--accent: #a798d7;\n      --border: #5fa8cc;"));
    assert!(html.contains("--exportPageBg: #21252c;"));
    assert!(html.contains("marked v18.0.5") && html.contains("Highlight.js v11.9.0"));
    for placeholder in ["{{CSS}}", "{{JS}}", "{{SESSION_DATA}}", "{{MARKED_JS}}", "{{THEME_VARS}}", "{{BODY_BG}}"] {
        assert!(!html.contains(placeholder), "{placeholder} left in page");
    }
    // Session text only travels base64-encoded, so it cannot close the script element.
    assert!(!html.contains("alert(\"xss\")"));
    assert!(!html.contains("onerror=alert(1)"));
    assert_eq!(html.matches("</script>").count(), 4);

    let data = session_data(&html);
    let keys: Vec<&String> = data.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["header", "entries", "leafId"]);
    assert_eq!(data["header"]["id"], "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b");
    assert_eq!(data["entries"].as_array().unwrap().len(), 12);
    assert_eq!(data["leafId"], "a1000012");
    let text = data["entries"][2]["message"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Escape test </script><script>alert(\"xss\")</script> & <b>bold</b> $& $' $` $$ {{JS}}"));
    assert!(text.ends_with("h\u{e9}llo \u{1f389}"));

    let start = html.find("type=\"application/json\">").unwrap() + 24;
    let end = start + html[start..].find('<').unwrap();
    let json = base64::engine::general_purpose::STANDARD.decode(&html[start..end]).unwrap();
    let json = String::from_utf8(json).unwrap();
    assert!(json.contains(r#""cost":{"input":3,"output":0.000001,"cacheRead":1e-7,"cacheWrite":0,"total":0.0015}"#));
    assert!(json.contains(r#""data":{"x":1,"y":[1,2.5,0]}"#));
    assert!(json.contains(r#""big":1.2345678901234569e+23"#));
    assert!(json.contains(r#""nestedCalls":{"calls":[{"id":"n1","name":"read""#));
}

#[test]
fn reports_missing_files_like_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.jsonl");
    let error = export_session_html(&missing, None, &HtmlExportOptions::default()).unwrap_err();
    assert_eq!(error.to_string(), "Nothing to export yet - start a conversation first");
    let error = export_from_file(missing.to_str().unwrap(), None, &ExportTheme::dark()).unwrap_err();
    assert_eq!(error.to_string(), format!("File not found: {}", missing.display()));

    let invalid = dir.path().join("invalid.jsonl");
    std::fs::write(&invalid, "not json\n").unwrap();
    let error = export_from_file(invalid.to_str().unwrap(), None, &ExportTheme::dark()).unwrap_err();
    assert_eq!(error.to_string(), format!("Session file is not a valid pi session: {}", invalid.display()));
}

#[test]
fn default_output_name_uses_the_session_file_name() {
    let path = Path::new("/sessions/2026-01-02T03-04-05-678Z_abc.jsonl");
    assert_eq!(default_html_output_path(path), PathBuf::from("pi-session-2026-01-02T03-04-05-678Z_abc.html"));
    assert_eq!(default_html_output_path(Path::new("/x/notes.txt")), PathBuf::from("pi-session-notes.txt.html"));
}
