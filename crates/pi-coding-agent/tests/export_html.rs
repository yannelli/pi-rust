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
use pi_js::rquickjs::{Context, Function, Runtime};
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
    assert_eq!(bytes.len(), 289_375);
    assert_eq!(fnv1a(&bytes), 0xda6e_0dc8_1e51_4862);
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
    assert_eq!(bytes.len(), 290_079);
    assert_eq!(fnv1a(&bytes), 0xa0cf_d0c5_7f3f_aa7a);

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
    assert!(html.contains("marked v18.0.11") && html.contains("Highlight.js v11.9.0"));
    assert!(html.contains("marked-cjk-friendly v0.1.2"));
    for placeholder in [
        "{{CSS}}",
        "{{JS}}",
        "{{SESSION_DATA}}",
        "{{MARKED_JS}}",
        "{{MARKED_CJK_FRIENDLY_JS}}",
        "{{THEME_VARS}}",
        "{{BODY_BG}}",
    ] {
        assert!(!html.contains(placeholder), "{placeholder} left in page");
    }
    // Session text only travels base64-encoded, so it cannot close the script element.
    assert!(!html.contains("alert(\"xss\")"));
    assert!(!html.contains("onerror=alert(1)"));
    assert_eq!(html.matches("</script>").count(), 5);

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

/// The DOM the viewer script touches while loading, stubbed for QuickJS.
const VIEWER_STUBS: &str = r#"
globalThis.window = { location: { search: "" } };
globalThis.document = { getElementById: () => ({ textContent: '{"header":{},"entries":[],"leafId":null}' }), querySelector: () => null };
globalThis.atob = (text) => text;
globalThis.TextDecoder = class { decode(bytes) { return String.fromCharCode(...bytes); } };
globalThis.URLSearchParams = class { get() { return null; } };
"#;

/// Run the exported page's scripts in QuickJS and render each of `inputs` with its `safeMarkedParse`.
fn render_markdown(html: &str, inputs: &[&str]) -> Vec<String> {
    let mut scripts = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script>") {
        let body = &rest[start + "<script>".len()..];
        let end = body.find("</script>").unwrap();
        scripts.push(&body[..end]);
        rest = &body[end..];
    }
    let viewer = scripts.pop().expect("viewer script");
    let viewer = viewer.replace("function safeMarkedParse(", "return safeMarkedParse; function safeMarkedParse(");
    let runtime = Runtime::new().unwrap();
    let context = Context::full(&runtime).unwrap();
    context.with(|ctx| {
        let run =
            |source: String| ctx.eval::<(), _>(source).unwrap_or_else(|error| panic!("{error}: {:?}", ctx.catch()));
        run(VIEWER_STUBS.to_string());
        for script in scripts {
            run(script.to_string());
        }
        let parse: Function = ctx.eval(viewer).unwrap_or_else(|error| panic!("{error}: {:?}", ctx.catch()));
        inputs.iter().map(|input| parse.call::<_, String>((*input,)).unwrap()).collect()
    })
}

#[test]
fn html_export_renders_emphasis_next_to_cjk_punctuation() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("cjk.html");
    export_from_file(fixture().to_str().unwrap(), out.to_str(), &ExportTheme::dark()).unwrap();
    let html = std::fs::read_to_string(&out).unwrap();

    let rendered = render_markdown(
        &html,
        &[
            "**这是测试。**后面",
            "**一句话说清：**一台机器",
            "**要点到此为止。**20 小时",
            "字**「引用」**字",
            "*斜体。*后面",
            // CommonMark flanking rules still apply to non-CJK text.
            "**end.**Then next",
        ],
    );
    assert_eq!(
        rendered,
        [
            "<p><strong>这是测试。</strong>后面</p>\n",
            "<p><strong>一句话说清：</strong>一台机器</p>\n",
            "<p><strong>要点到此为止。</strong>20 小时</p>\n",
            "<p>字<strong>「引用」</strong>字</p>\n",
            "<p><em>斜体。</em>后面</p>\n",
            "<p>**end.**Then next</p>\n",
        ]
    );
}

const TEMPLATE_JS: &str = include_str!("../src/export/assets/template.js");

/// The template's system message helpers, run in QuickJS against `entries` (upstream
/// `export-html-system-messages.test.ts`). Returns each call's string or boolean result as JSON.
fn system_helpers(entries: &Value, calls: &str) -> Value {
    let start = TEMPLATE_JS.find("function systemContentText(content)").expect("helpers start");
    let end = TEMPLATE_JS.find("const hasSystemEntries").expect("helpers end");
    assert!(end > start);
    let source = format!(
        "const entries = {entries};\nconst byId = new Map(entries.map(entry => [entry.id, entry]));\n{}\nJSON.stringify({calls})",
        &TEMPLATE_JS[start..end]
    );
    let runtime = Runtime::new().unwrap();
    let context = Context::full(&runtime).unwrap();
    let json: String =
        context.with(|ctx| ctx.eval(source).unwrap_or_else(|error| panic!("{error}: {:?}", ctx.catch())));
    serde_json::from_str(&json).unwrap()
}

fn system_messages() -> (Value, Value) {
    let initial = json!({
        "role": "system",
        "content": "You are a helpful assistant.",
        "sections": {"rules": "<rules>\nbe concise\n</rules>", "project_context": "<project_context>\nv1\n</project_context>"},
        "toolsAdded": [{"name": "read", "description": "Read a file", "parameters": {"type": "object", "properties": {}}}],
        "timestamp": 1,
    });
    let update = json!({
        "role": "system",
        "content": [{"type": "text", "text": "Extra instructions"}],
        "sections": {"project_context": "<project_context>\nv2\n</project_context>", "rules": null},
        "toolsAdded": [{"name": "bash", "description": "Run a command", "parameters": {"type": "object", "properties": {}}}],
        "toolsRemoved": [{"name": "read"}],
        "timestamp": 2,
    });
    (initial, update)
}

// Regression tests for upstream #11261: system messages were stored in the export but never rendered.
#[test]
fn export_renders_system_message_text_the_way_the_model_receives_it() {
    let (initial, update) = system_messages();
    let entries = json!([{"type": "message", "id": "s1", "parentId": null, "message": initial}]);
    let calls = format!("[getSystemMessageText({initial}), renderSystemMessageUpdate({update})]");
    let rendered = system_helpers(&entries, &calls);
    let initial: pi_ai::SystemMessage = serde_json::from_value(initial).unwrap();
    let update: pi_ai::SystemMessage = serde_json::from_value(update).unwrap();
    assert_eq!(rendered[0], pi_ai::transcript::system_message_text(&initial));
    assert_eq!(rendered[1], pi_ai::transcript::render_system_message_update(&update));
}

#[test]
fn export_renders_the_first_system_entry_as_the_prompt_and_later_ones_as_updates() {
    let (initial, update) = system_messages();
    let entries = json!([
        {"type": "message", "id": "s1", "parentId": null, "message": initial},
        {"type": "message", "id": "u1", "parentId": "s1", "message": {"role": "user", "content": "hi"}},
        {"type": "message", "id": "s2", "parentId": "u1", "message": update},
    ]);
    let calls =
        format!("[isLeadingSystemEntry(entries[0]), isLeadingSystemEntry(entries[2]), describeSystemUpdate({update})]");
    assert_eq!(
        system_helpers(&entries, &calls),
        json!([true, false, "instructions, project_context, -rules, tools +bash -read"])
    );

    let entries = json!([
        {"type": "compaction", "id": "c1", "parentId": null, "systemMessage": initial},
        {"type": "message", "id": "s2", "parentId": "c1", "message": update},
    ]);
    let calls = "[isLeadingSystemEntry({ id: 's2', parentId: 'c1' })]";
    assert_eq!(system_helpers(&entries, calls), json!([false]));

    let opener = "if (msg.role === 'system') {";
    let renders_entries = TEMPLATE_JS
        .match_indices(opener)
        .any(|(at, _)| TEMPLATE_JS[at + opener.len()..].trim_start().starts_with("let html = '';"));
    assert!(renders_entries);
    assert!(TEMPLATE_JS.contains("renderSystemPromptBlock('System Prompt', text)"));
    assert!(TEMPLATE_JS.contains("renderToolsList('Available Tools', msg.toolsAdded)"));
}

/// The tree functions of an exported page's viewer script, loaded in QuickJS (upstream
/// `export-html-deep-tree.test.ts`). The session data is decoded here, so `atob` passes it on.
const TREE_STUBS: &str = r#"
globalThis.window = { location: { search: "" } };
globalThis.document = { getElementById: () => ({ textContent: globalThis.sessionJson }), querySelector: () => null };
globalThis.atob = (text) => text;
globalThis.TextDecoder = class {
  decode(bytes) {
    let text = "";
    for (let i = 0; i < bytes.length; i += 8192) text += String.fromCharCode.apply(null, bytes.subarray(i, i + 8192));
    return text;
  }
};
globalThis.URLSearchParams = class { get() { return null; } };
"#;

// Upstream #11225: recursive tree walks overflowed the call stack on long sessions.
#[test]
fn html_export_renders_sessions_deeper_than_the_call_stack() {
    const DEPTH: usize = 20_000;
    let dir = tempfile::tempdir().unwrap();
    let header = json!({"type": "session", "version": 3, "id": "deep-test", "timestamp": "1970-01-01T00:00:00.000Z", "cwd": "/tmp"});
    let mut lines = vec![header.to_string()];
    for i in 0..DEPTH {
        let timestamp = format!("1970-01-01T{:02}:{:02}:{:02}.000Z", i / 3600, i / 60 % 60, i % 60);
        let parent = if i == 0 { Value::Null } else { json!(format!("e{}", i - 1)) };
        let message = json!({"role": "user", "content": "x", "timestamp": i * 1000});
        let entry = json!({"type": "message", "id": format!("e{i}"), "parentId": parent, "timestamp": timestamp, "message": message});
        lines.push(entry.to_string());
    }
    let session = dir.path().join("session.jsonl");
    std::fs::write(&session, lines.join("\n") + "\n").unwrap();
    let out = dir.path().join("out.html");
    export_from_file(session.to_str().unwrap(), out.to_str(), &ExportTheme::dark()).unwrap();
    let html = std::fs::read_to_string(&out).unwrap();

    let session_json = stringify(&session_data(&html));
    let viewer_start = html.rfind("<script>").unwrap() + "<script>".len();
    let viewer = &html[viewer_start..viewer_start + html[viewer_start..].find("</script>").unwrap()];
    let api = "return { buildTree, buildActivePathIds, flattenTree, findNewestLeaf, getPath };";
    let viewer = viewer.replace("let treeNodeMap = null;", &format!("let treeNodeMap = null; {api}"));
    let checks = r#"
const tree = api.buildTree();
const leafId = `e${depth - 1}`;
const flat = api.flattenTree(tree, api.buildActivePathIds(leafId));
const path = api.getPath(leafId);
JSON.stringify([tree.length, flat.length, flat[depth - 1].node.entry.id, api.findNewestLeaf("e0"), path.length, path[0].id, path[depth - 1].id])
"#;
    let runtime = Runtime::new().unwrap();
    let context = Context::full(&runtime).unwrap();
    let result: String = context.with(|ctx| {
        let run =
            |source: String| ctx.eval::<(), _>(source).unwrap_or_else(|error| panic!("{error}: {:?}", ctx.catch()));
        ctx.globals().set("sessionJson", session_json).unwrap();
        ctx.globals().set("depth", DEPTH).unwrap();
        run(TREE_STUBS.to_string());
        run(format!("globalThis.api = {viewer}"));
        ctx.eval(checks).unwrap_or_else(|error| panic!("{error}: {:?}", ctx.catch()))
    });
    let leaf = format!("e{}", DEPTH - 1);
    assert_eq!(serde_json::from_str::<Value>(&result).unwrap(), json!([1, DEPTH, leaf, leaf, DEPTH, "e0", leaf]));
}
