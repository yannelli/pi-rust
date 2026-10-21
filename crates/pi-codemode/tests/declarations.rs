//! Ported from upstream `packages/codemode/test/declarations.test.ts`.

use pi_codemode::{ToolDeclaration, render_declarations, render_tool_sample, render_tool_signature, schema_to_type};
use serde_json::{Value, json};

fn ty(schema: Value) -> String {
    schema_to_type(&schema, None)
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
fn keeps_the_sibling_type_of_all_of_constraints() {
    // Zod 4 renders a string with several `.regex()` checks this way.
    let patterns = json!({ "type": "string", "allOf": [{ "pattern": "^a" }, { "pattern": "z$" }] });
    assert_eq!(ty(patterns.clone()), "string");
    let edit = json!({ "type": "object", "properties": { "find": patterns }, "required": ["find"] });
    assert_eq!(ty(edit), "{ find: string; }");
    let object = json!({
        "type": "object",
        "properties": { "find": { "type": "string" } },
        "required": ["find"],
        "allOf": [{ "properties": { "replace": { "type": "string" } } }],
    });
    assert_eq!(ty(object), "{ find: string; } & { replace?: string; }");
    let bare = json!({ "type": "object", "allOf": [{ "properties": { "a": { "type": "string" } } }] });
    assert_eq!(ty(bare), "{ a?: string; }");
    assert_eq!(ty(json!({ "type": "object", "allOf": [{}] })), "{ [key: string]: unknown; }");
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

// https://github.com/earendil-works/pi/issues/10707
#[test]
fn puts_constraints_the_type_cannot_express_on_property_comment_lines() {
    let schema = json!({
        "type": "object",
        "properties": {
            "timeoutMs": {
                "type": "integer", "minimum": 0, "maximum": 30000, "default": 0, "description": "Wait in milliseconds.",
            },
            "id": { "type": "string", "pattern": "^[a-z]+$", "maxLength": 8 },
            "tags": { "type": "array", "items": { "type": "string" }, "maxItems": 5, "uniqueItems": true, "deprecated": true },
            "name": { "type": "string" },
        },
        "required": ["id"],
    });
    assert_eq!(
        ty(schema),
        [
            "{",
            "  // @maxLength 8 @pattern ^[a-z]+$",
            "  id: string;",
            "  name?: string;",
            "  // @maxItems 5 @uniqueItems @deprecated",
            "  tags?: Array<string>;",
            "  // Wait in milliseconds.",
            "  // @integer @minimum 0 @maximum 30000 @default 0",
            "  timeoutMs?: number;",
            "}",
        ]
        .join("\n")
    );
}

#[test]
fn reads_constraints_of_optional_fields_from_the_non_null_branch() {
    // Pydantic's `limit: int | None = Field(None, ge=1, le=100)`.
    let limit = json!({
        "anyOf": [{ "type": "integer", "minimum": 1, "maximum": 100 }, { "type": "null" }],
        "default": null,
        "title": "Limit",
    });
    assert_eq!(
        ty(json!({ "type": "object", "properties": { "limit": limit } })),
        "{\n  // @integer @minimum 1 @maximum 100\n  limit?: number | null;\n}"
    );
    // Constraints of the property and the branch both apply.
    let n = json!({
        "anyOf": [{ "type": "integer", "minimum": 10, "maximum": 50, "format": "int32" }, { "type": "null" }],
        "minimum": 0,
        "maximum": 20,
        "format": "port",
    });
    assert_eq!(
        ty(json!({ "type": "object", "properties": { "n": n } })),
        "{\n  // @integer @minimum 10 @maximum 20 @format port @format int32\n  n?: number | null;\n}"
    );
}

#[test]
fn reads_draft_04_boolean_exclusive_bounds() {
    let schema = json!({
        "type": "object",
        "properties": {
            "n": { "type": "integer", "minimum": 0, "exclusiveMinimum": true, "maximum": 10, "exclusiveMaximum": false },
            "m": { "anyOf": [{ "type": "number", "maximum": 5, "exclusiveMaximum": true }, { "type": "null" }], "maximum": 8 },
        },
    });
    assert_eq!(
        ty(schema),
        [
            "{",
            "  // @maximum 8 @exclusiveMaximum 5",
            "  m?: number | null;",
            "  // @integer @exclusiveMinimum 0 @maximum 10",
            "  n?: number;",
            "}",
        ]
        .join("\n")
    );
}

#[test]
fn leaves_out_long_tags_and_escapes_line_terminators() {
    let config = json!({ "type": "object", "properties": { "config": { "type": "object", "default": { "text": "x".repeat(100) } } } });
    assert_eq!(ty(config), "{ config?: { [key: string]: unknown; }; }");
    let sep = json!({
        "type": "string", "pattern": "^[\r\n\u{2028}\u{2029}]$", "default": "\u{2028}", "description": "a\u{2028}b",
    });
    assert_eq!(
        ty(json!({ "type": "object", "properties": { "sep": sep } })),
        [
            "{",
            "  // a",
            "  // b",
            "  // @pattern ^[\\u000d\\u000a\\u2028\\u2029]$ @default \"\\u2028\"",
            "  sep?: string;",
            "}",
        ]
        .join("\n")
    );
}

#[test]
fn drops_constraint_tags_before_rendering_a_type_over_the_budget_as_unknown() {
    let schema = json!({ "type": "object", "properties": { "n": { "type": "integer", "minimum": 1 } } });
    assert_eq!(schema_to_type(&schema, None), "{\n  // @integer @minimum 1\n  n?: number;\n}");
    assert_eq!(schema_to_type(&schema, Some(20)), "{ n?: number; }");
    assert_eq!(schema_to_type(&schema, Some(10)), "unknown");
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
fn renders_output_schemas_shaped_like_an_mcp_call_tool_result_as_plain_types() {
    let output = json!({
        "type": "object",
        "properties": {
            "content": { "type": "array", "items": { "type": "object" } },
            "isError": { "type": "boolean" },
            "_meta": { "type": "object" },
        },
        "required": ["content"],
    });
    let plain = declaration("plain", Some(json!({ "type": "object", "properties": {} })), Some(output));
    assert_eq!(
        render_tool_signature(&plain, None),
        "plain(args: { [key: string]: unknown; }): Promise<{ _meta?: { [key: string]: unknown; }; content: Array<{ [key: string]: unknown; }>; isError?: boolean; }>;"
    );
}

/// What `pi_mcp::tools::create_mcp_result_schema` returns, as a tool's output schema.
fn mcp_result_schema(structured_content: Option<Value>) -> Value {
    let block = json!({
        "type": "object",
        "properties": {
            "type": { "enum": ["text", "image", "audio", "resource_link", "resource"] },
            "text": { "type": "string" },
            "data": { "type": "string" },
            "mimeType": { "type": "string" },
        },
        "required": ["type"],
        "additionalProperties": true,
    });
    json!({
        "type": "object",
        "properties": {
            "content": { "type": "array", "items": block },
            "structuredContent": structured_content.unwrap_or_else(|| json!({})),
            "isError": { "type": "boolean" },
        },
        "required": ["content"],
        "additionalProperties": true,
    })
}

#[test]
fn renders_the_mcp_result_schema_with_references_into_the_servers_output_schema() {
    let content = "content: Array<{ data?: string; mimeType?: string; text?: string; type: \"text\" | \"image\" | \"audio\" | \"resource_link\" | \"resource\"; [key: string]: unknown; }>; isError?: boolean;";
    assert_eq!(
        schema_to_type(&mcp_result_schema(None), None),
        format!("{{ {content} structuredContent?: unknown; [key: string]: unknown; }}")
    );

    let hit = json!({ "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] });
    let prefix = "#/properties/structuredContent";
    let structured = json!({
        "type": "object",
        "properties": {
            "hits": { "type": "array", "items": { "$ref": format!("{prefix}/$defs/Hit~1v1") } },
            "again": { "$ref": format!("{prefix}/properties/hits") },
            "children": { "type": "array", "items": { "$ref": prefix } },
            "default": { "$ref": format!("{prefix}/definitions/Hit") },
            "remote": { "$ref": "https://example.com/schema.json" },
        },
        "required": ["hits"],
        "$defs": { "Hit/v1": hit },
        "definitions": { "Hit": hit },
    });
    assert_eq!(
        schema_to_type(&mcp_result_schema(Some(structured)), None),
        format!(
            "{{ {content} structuredContent?: {{ again?: Array<{{ id: string; }}>; children?: Array<{{ again?: Array<{{ id: string; }}>; children?: Array<unknown>; default?: {{ id: string; }}; hits: Array<{{ id: string; }}>; remote?: unknown; }}>; default?: {{ id: string; }}; hits: Array<{{ id: string; }}>; remote?: unknown; }}; [key: string]: unknown; }}"
        )
    );
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
