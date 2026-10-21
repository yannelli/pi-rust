//! Result conversion and truncation, ported from the "MCP tools" cases of `mcp-extension.test.ts`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pi_mcp::output::{ConvertOptions, OutputSaver, truncate_middle};
use pi_mcp::tools::create_mcp_result_schema;
use pi_mcp::{CallToolResult, convert_mcp_result};
use serde_json::{Value, json};

fn result(value: Value) -> CallToolResult {
    serde_json::from_value(value).unwrap()
}

fn content(output: &pi_mcp::McpToolOutput) -> Value {
    serde_json::to_value(&output.content).unwrap()
}

type Saved = Arc<Mutex<Vec<(Vec<u8>, String)>>>;

fn recording_saver(path: &'static str) -> (OutputSaver, Saved) {
    let saved = Arc::new(Mutex::new(Vec::new()));
    let sink = saved.clone();
    let saver: OutputSaver = Arc::new(move |data: &[u8], extension: &str| {
        sink.lock().unwrap().push((data.to_vec(), extension.to_string()));
        Ok(PathBuf::from(format!("{path}{extension}")))
    });
    (saver, saved)
}

#[test]
fn converts_results_passing_the_call_tool_result_to_scripts_and_flagging_errors() {
    let blocks = json!([
        { "type": "resource_link", "uri": "file:///a", "name": "a" },
        { "type": "resource", "resource": { "uri": "file:///b", "text": "b text" } },
        { "type": "audio", "data": "", "mimeType": "audio/wav" }
    ]);
    let options = ConvertOptions::default();
    let converted = convert_mcp_result(
        "docs",
        "t",
        &result(json!({ "content": blocks, "structuredContent": { "ok": true }, "_meta": { "trace": "x" } })),
        &options,
    );
    assert_eq!(
        content(&converted),
        json!([
            { "type": "text", "text": "[Resource file:///a \"a\"]" },
            { "type": "text", "text": "b text" },
            { "type": "text", "text": "[audio audio/wav omitted]" }
        ])
    );
    assert_eq!(serde_json::to_value(&converted.details).unwrap(), json!({ "server": "docs", "tool": "t" }));
    // Scripts get the server's blocks as sent, without `_meta`.
    assert_eq!(converted.structured_content, json!({ "content": blocks, "structuredContent": { "ok": true } }));
    assert!(!converted.is_error);

    let structured =
        convert_mcp_result("docs", "t", &result(json!({ "content": [], "structuredContent": { "n": 1 } })), &options);
    assert_eq!(content(&structured), json!([{ "type": "text", "text": "{\n  \"n\": 1\n}" }]));

    let failed = convert_mcp_result(
        "docs",
        "t",
        &result(json!({ "content": [{ "type": "text", "text": "nope" }], "isError": true })),
        &options,
    );
    assert_eq!(content(&failed), json!([{ "type": "text", "text": "nope" }]));
    assert_eq!(failed.structured_content, json!({ "content": [{ "type": "text", "text": "nope" }], "isError": true }));
    assert!(failed.is_error);
    let empty = convert_mcp_result("docs", "t", &result(json!({ "content": [], "isError": true })), &options);
    assert_eq!(content(&empty), json!([{ "type": "text", "text": "MCP tool docs/t returned an error" }]));
}

#[test]
fn points_resource_links_to_read_mcp_resource_and_saves_binary_resources() {
    let (saver, saved) = recording_saver("/tmp/saved");
    let options = ConvertOptions { saver, readable_resources: true };
    let converted = convert_mcp_result(
        "docs",
        "t",
        &result(json!({ "content": [
            { "type": "resource_link", "uri": "docs://guide", "name": "guide", "title": "The Guide", "mimeType": "text/markdown", "size": 2048, "description": "How to use it" },
            { "type": "resource", "resource": { "uri": "file:///r/report.pdf", "mimeType": "application/pdf", "blob": "JVBERg==" } },
            { "type": "resource", "resource": { "uri": "docs://logo", "mimeType": "image/png", "blob": "AAAA" } },
            { "type": "resource", "resource": { "uri": "docs://data", "mimeType": "application/json; charset=utf-8", "blob": "eyJhIjoxfQ==" } }
        ] })),
        &options,
    );
    assert_eq!(
        content(&converted),
        json!([
            { "type": "text", "text": "[Resource docs://guide \"The Guide\" (text/markdown, 2.0KB): How to use it. Read it with read_mcp_resource (server \"docs\")]" },
            { "type": "text", "text": "[Binary resource file:///r/report.pdf (application/pdf, 4B) saved to /tmp/saved.pdf]" },
            { "type": "image", "data": "AAAA", "mimeType": "image/png" },
            { "type": "text", "text": "{\"a\":1}" }
        ])
    );
    assert_eq!(*saved.lock().unwrap(), vec![(b"%PDF".to_vec(), ".pdf".to_string())]);
}

#[test]
fn cuts_the_middle_of_model_facing_text_over_20kb_and_keeps_the_full_result_for_scripts() {
    let (saver, saved) = recording_saver("/tmp/full");
    let options = ConvertOptions { saver, readable_resources: false };
    let full: String = (1..=3000).map(|index| format!("line {index}")).collect::<Vec<_>>().join("\n");
    let call = result(
        json!({ "content": [{ "type": "text", "text": full }, { "type": "image", "data": "AAAA", "mimeType": "image/png" }] }),
    );
    let converted = convert_mcp_result("docs", "snapshot", &call, &options);
    assert_eq!(converted.content.len(), 2);
    let text = converted.content[0].as_text().unwrap();
    let header = format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: 3000\n\nline 1\nline 2\n",
        full.len().div_ceil(4)
    );
    assert!(text.starts_with(&header), "{}", &text[..200]);
    assert!(text.contains(" chars truncated…"));
    assert!(text.ends_with("line 3000\n\n[Full output: /tmp/full.txt (read it with offset/limit)]"));
    assert!(text.len() < 21 * 1024);
    assert_eq!(
        serde_json::to_value(&converted.content[1]).unwrap(),
        json!({ "type": "image", "data": "AAAA", "mimeType": "image/png" })
    );
    assert_eq!(
        serde_json::to_value(&converted.details).unwrap(),
        json!({ "server": "docs", "tool": "snapshot", "fullOutputPath": "/tmp/full.txt" })
    );
    assert_eq!(saved.lock().unwrap().len(), 1);
    assert_eq!(saved.lock().unwrap()[0].0, full.as_bytes());
    assert_eq!(converted.structured_content, serde_json::to_value(&call).unwrap());

    // Text within the limit is not saved.
    convert_mcp_result("docs", "small", &result(json!({ "content": [{ "type": "text", "text": "ok" }] })), &options);
    assert_eq!(saved.lock().unwrap().len(), 1);
}

#[test]
fn cuts_multi_byte_text_only_at_character_boundaries() {
    let text = format!("{}end", "é".repeat(20_000));
    let result = truncate_middle(&text, 1001);
    assert!(result.truncated);
    assert!(!result.content.contains('\u{FFFD}'));
    assert!(result.content.ends_with("end"));
    let marker = format!("…{} chars truncated…", result.removed_chars);
    let (head, tail) = result.content.split_once(&marker).unwrap();
    assert!(head.len() <= 500);
    assert!(tail.len() <= 501);
    assert_eq!(head.chars().count() + tail.chars().count() + result.removed_chars, text.chars().count());
}

#[test]
fn declares_the_call_tool_result_scripts_receive_keeping_references_in_the_servers_output_schema() {
    let undeclared = create_mcp_result_schema(None);
    assert_eq!(undeclared["properties"]["structuredContent"], json!({ "type": "object" }));
    assert_eq!(undeclared["required"], json!(["content"]));
    assert_eq!(undeclared["additionalProperties"], json!(true));
    assert!(undeclared["properties"].get("_meta").is_none());
    let block = &undeclared["properties"]["content"]["items"];
    assert_eq!(block["properties"]["type"]["enum"], json!(["text", "image", "audio", "resource_link", "resource"]));
    assert_eq!(block["required"], json!(["type"]));
    let fields: Vec<&String> = undeclared["properties"].as_object().unwrap().keys().collect();
    assert_eq!(fields, ["content", "structuredContent", "isError"]);

    let hit = json!({ "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] });
    let mut defined = hit.clone();
    defined["default"] = json!({ "$ref": "#/stays" });
    let schema = create_mcp_result_schema(Some(&json!({
        "type": "object",
        "properties": {
            "hits": { "type": "array", "items": { "$ref": "#/$defs/Hit~1v1" } },
            "again": { "$ref": "#/properties/hits" },
            "children": { "type": "array", "items": { "$ref": "#" } },
            // A property named like a keyword whose value is data.
            "default": { "$ref": "#/definitions/Hit" },
            "remote": { "$ref": "https://example.com/schema.json" },
        },
        "required": ["hits"],
        "$defs": { "Hit/v1": defined },
        "definitions": { "Hit": hit },
    })));
    let structured = &schema["properties"]["structuredContent"];
    let prefix = "#/properties/structuredContent";
    let reference = |value: &Value| value["$ref"].as_str().unwrap().to_string();
    assert_eq!(reference(&structured["properties"]["hits"]["items"]), format!("{prefix}/$defs/Hit~1v1"));
    assert_eq!(reference(&structured["properties"]["again"]), format!("{prefix}/properties/hits"));
    assert_eq!(reference(&structured["properties"]["children"]["items"]), prefix);
    assert_eq!(reference(&structured["properties"]["default"]), format!("{prefix}/definitions/Hit"));
    assert_eq!(reference(&structured["properties"]["remote"]), "https://example.com/schema.json");
    // Data that looks like a reference is kept as is.
    assert_eq!(structured["$defs"]["Hit/v1"]["default"], json!({ "$ref": "#/stays" }));
    assert_eq!(structured["required"], json!(["hits"]));
}
