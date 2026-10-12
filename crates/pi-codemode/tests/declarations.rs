//! Ported from upstream `packages/codemode/test/declarations.test.ts`.

use pi_codemode::{
    ToolDeclaration, mcp_structured_content_schema, render_declarations, render_tool_sample, render_tool_signature,
    schema_to_type,
};
use serde_json::{Value, json};

fn ty(schema: Value) -> String {
    schema_to_type(&schema, None)
}

fn mcp_result_schema(structured_content: Option<Value>) -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert("content".into(), json!({ "type": "array", "items": { "type": "object" } }));
    if let Some(structured) = structured_content {
        properties.insert("structuredContent".into(), structured);
    }
    properties.insert("isError".into(), json!({ "type": "boolean" }));
    properties.insert("_meta".into(), json!({ "type": "object" }));
    json!({ "type": "object", "properties": properties, "required": ["content"] })
}

fn declaration(name: &str, input: Option<Value>, output: Option<Value>) -> ToolDeclaration {
    ToolDeclaration { name: name.into(), input_schema: input, output_schema: output, ..ToolDeclaration::default() }
}

#[test]
fn renders_primitives_literals_and_unions() {
    assert_eq!(ty(json!({ "type": "string" })), "string");
    assert_eq!(ty(json!({ "type": "integer" })), "number");
    assert_eq!(ty(json!({ "type": ["string", "null"] })), "string | null");
    assert_eq!(ty(json!({ "const": "a" })), "\"a\"");
    assert_eq!(ty(json!({ "enum": ["a", 1, null] })), "\"a\" | 1 | null");
    assert_eq!(ty(json!({ "anyOf": [{ "type": "string" }, { "type": "number" }] })), "string | number");
    assert_eq!(ty(json!({ "anyOf": [{ "type": "string" }, {}] })), "unknown");
    assert_eq!(
        ty(json!({ "allOf": [{ "anyOf": [{ "type": "string" }, { "type": "number" }] }, { "const": 1 }] })),
        "(string | number) & 1"
    );
    assert_eq!(ty(json!({ "$ref": "#/defs/x" })), "unknown");
    assert_eq!(ty(json!(true)), "unknown");
    assert_eq!(ty(json!(false)), "never");
}

#[test]
fn renders_objects_on_one_line_with_sorted_properties() {
    let schema = json!({
        "type": "object",
        "properties": { "city": { "type": "string" }, "max-lines": { "type": "number" } },
        "required": ["city"],
        "additionalProperties": false,
    });
    assert_eq!(ty(schema), "{ city: string; \"max-lines\"?: number; }");
    assert_eq!(
        ty(json!({ "type": "object", "additionalProperties": { "type": "number" } })),
        "{ [key: string]: number; }"
    );
    assert_eq!(ty(json!({ "type": "object" })), "{ [key: string]: unknown; }");
    assert_eq!(ty(json!({ "type": "object", "properties": {}, "additionalProperties": false })), "{}");
}

#[test]
fn puts_property_descriptions_on_comment_lines() {
    let weather = json!({
        "type": "object",
        "properties": {
            "weather": {
                "type": "array",
                "description": "look up weather for a given list of locations",
                "items": { "type": "object", "properties": { "location": { "type": "string" } }, "required": ["location"] },
            },
        },
        "required": ["weather"],
    });
    assert_eq!(
        ty(weather),
        "{\n  // look up weather for a given list of locations\n  weather: Array<{ location: string; }>;\n}"
    );
    let nested = json!({
        "type": "object",
        "properties": {
            "outer": {
                "type": "object",
                "description": "Outer",
                "properties": { "inner": { "type": "string", "description": "Inner" } },
            },
        },
    });
    assert_eq!(ty(nested), "{\n  // Outer\n  outer?: {\n    // Inner\n    inner?: string;\n  };\n}");
}

#[test]
fn resolves_local_references_and_stops_at_recursive_ones() {
    let schema = json!({
        "type": "object",
        "properties": {
            "item": { "$ref": "#/$defs/Item" },
            "legacy": { "$ref": "#/definitions/Legacy" },
            "remote": { "$ref": "https://example.com/schema.json" },
        },
        "required": ["item"],
        "$defs": {
            "Item": {
                "type": "object",
                "properties": { "id": { "type": "string" }, "parent": { "$ref": "#/$defs/Item" } },
                "required": ["id"],
            },
        },
        "definitions": { "Legacy": { "enum": ["a", "b"] } },
    });
    assert_eq!(ty(schema), "{ item: { id: string; parent?: unknown; }; legacy?: \"a\" | \"b\"; remote?: unknown; }");
}

#[test]
fn renders_arrays_and_tuples() {
    assert_eq!(ty(json!({ "type": "array", "items": { "type": "string" } })), "Array<string>");
    assert_eq!(
        ty(json!({ "type": "array", "prefixItems": [{ "type": "string" }, { "type": "number" }] })),
        "[string, number]"
    );
    assert_eq!(ty(json!({ "type": "array" })), "unknown[]");
}

#[test]
fn renders_types_over_the_budget_as_unknown() {
    let properties: serde_json::Map<String, Value> =
        (0..50).map(|i| (format!("field{i}"), json!({ "type": "string" }))).collect();
    let schema = json!({ "type": "object", "properties": properties });
    assert_eq!(schema_to_type(&schema, Some(100)), "unknown");
    assert!(schema_to_type(&schema, None).contains("field49?: string;"));
}

#[test]
fn renders_signatures_with_normalized_identifiers() {
    let tool = declaration(
        "hidden-dynamic-tool",
        Some(json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
            "additionalProperties": false,
        })),
        Some(json!({ "type": "object", "properties": { "ok": { "type": "boolean" } }, "required": ["ok"] })),
    );
    assert_eq!(
        render_tool_signature(&tool, None),
        "hidden_dynamic_tool(args: { city: string; }): Promise<{ ok: boolean; }>;"
    );
    assert_eq!(render_tool_signature(&declaration("free", None, None), None), "free(args: unknown): Promise<unknown>;");
}

#[test]
fn renders_mcp_call_tool_result_output_schemas() {
    let input = json!({ "type": "object", "properties": {}, "additionalProperties": false });
    let structured = json!({
        "type": "object",
        "properties": { "results": { "type": "array", "items": { "$ref": "#/definitions/Result~1item~0v1" } } },
        "required": ["results"],
        "additionalProperties": false,
        "definitions": {
            "Result/item~v1": {
                "type": "object",
                "properties": { "id": { "type": "string" }, "score": { "type": "number" } },
                "required": ["id", "score"],
                "additionalProperties": false,
            },
        },
    });
    let search = declaration("mcp__sample__search", Some(input.clone()), Some(mcp_result_schema(Some(structured))));
    assert_eq!(
        render_tool_signature(&search, None),
        "mcp__sample__search(args: {}): Promise<CallToolResult<{ results: Array<{ id: string; score: number; }>; }>>;"
    );
    let plain = declaration("plain", Some(input), Some(mcp_result_schema(None)));
    assert_eq!(render_tool_signature(&plain, None), "plain(args: {}): Promise<CallToolResult>;");
    let not_mcp = json!({ "type": "object", "properties": { "content": { "type": "array" } } });
    assert_eq!(mcp_structured_content_schema(Some(&not_mcp)), None);
}

#[test]
fn renders_the_per_tool_sample() {
    let tool = ToolDeclaration {
        description: Some("bar".into()),
        ..declaration("foo", Some(json!({ "type": "string" })), None)
    };
    assert_eq!(
        render_tool_sample(&tool, None),
        "bar\n\ncodemode tool declaration:\n```ts\ndeclare const tools: { foo(args: string): Promise<unknown>; };\n```"
    );
}

#[test]
fn renders_tools_and_globals() {
    let read = ToolDeclaration {
        description: Some("Read a file.\nSecond line.".into()),
        ..declaration(
            "read",
            Some(json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] })),
            Some(json!({ "type": "string" })),
        )
    };
    let attach = ToolDeclaration {
        description: Some("Attach it.".into()),
        ..declaration("attach", Some(json!({ "type": "string" })), None)
    };
    let text = render_declarations(&[read, declaration("remote-api", None, None)], &[attach]);
    assert_eq!(
        text,
        [
            "declare const tools: {",
            "  /**",
            "   * Read a file.",
            "   * Second line.",
            "   */",
            "  read(args: { path: string; }): Promise<string>;",
            "  remote_api(args: unknown): Promise<unknown>;",
            "};",
            "",
            "/** Attach it. */",
            "declare function attach(args: string): Promise<unknown>;",
        ]
        .join("\n")
    );
}

#[test]
fn renders_namespaced_globals_and_explicit_signatures() {
    let list = ToolDeclaration {
        description: Some("List models.".into()),
        signature: Some("(type: string): Promise<string[]>".into()),
        ..declaration("models.list", None, None)
    };
    let get = declaration("models.get", Some(json!({ "type": "string" })), None);
    let plain = ToolDeclaration { signature: Some("(): void".into()), ..declaration("plain", None, None) };
    assert_eq!(
        render_declarations(&[], &[list, get, plain]),
        [
            "declare function plain(): void;",
            "",
            "declare const models: {",
            "  /** List models. */",
            "  list(type: string): Promise<string[]>;",
            "  get(args: string): Promise<unknown>;",
            "};",
        ]
        .join("\n")
    );
}

#[test]
fn escapes_comment_terminators_in_descriptions() {
    let tool = ToolDeclaration { description: Some("a */ b".into()), ..declaration("x", None, None) };
    assert!(render_declarations(&[tool], &[]).contains("/** a *\\/ b */"));
}
