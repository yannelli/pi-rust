//! Message and tool conversion shared by the Gemini APIs (upstream `google-shared.ts`).

use serde_json::{Map, Value, json};

use crate::providers::common::{sanitize_id, take_chars, transform_messages};
use crate::providers::constrained_sampling::{json_schema_tool_parameters, resolve_json_schema_strict_sampling};
use crate::types::{AssistantMessage, ContentBlock, Context, Message, Model, Tool, ToolResultMessage, UserContent};

const JSON_SCHEMA_META_KEYS: [&str; 8] =
    ["$schema", "$id", "$anchor", "$dynamicAnchor", "$vocabulary", "$comment", "$defs", "definitions"];

/// `N` in `gemini-N...` or `gemini-live-N...`.
fn gemini_major_version(model_id: &str) -> Option<u32> {
    let id = model_id.to_lowercase();
    let rest = id.strip_prefix("gemini-")?;
    let rest = rest.strip_prefix("live-").unwrap_or(rest);
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Models behind Google APIs that need explicit ids on function calls and responses.
pub(crate) fn requires_tool_call_id(model_id: &str) -> bool {
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || gemini_major_version(model_id).is_some_and(|major| major >= 3)
}

fn supports_multimodal_function_response(model_id: &str) -> bool {
    gemini_major_version(model_id).is_none_or(|major| major >= 3)
}

/// Gemini 3+ enforces required function parameters in validated tool-calling modes.
pub(crate) fn supports_strict_tool_sampling(model_id: &str) -> bool {
    gemini_major_version(model_id).is_some_and(|major| major >= 3)
}

/// Thought signatures are base64 (`TYPE_BYTES`); only same-model ones are replayed.
fn resolve_signature(same_model: bool, signature: Option<&str>) -> Option<&str> {
    let signature = signature.filter(|_| same_model)?;
    let valid_chars =
        signature.trim_end_matches('=').chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/');
    let padding = signature.len() - signature.trim_end_matches('=').len();
    (!signature.is_empty() && signature.len() % 4 == 0 && padding <= 2 && valid_chars && padding < signature.len())
        .then_some(signature)
}

fn with_signature(mut part: Value, signature: Option<&str>) -> Value {
    if let Some(signature) = signature {
        part["thoughtSignature"] = json!(signature);
    }
    part
}

fn inline_data(mime_type: &str, data: &str) -> Value {
    json!({"inlineData": {"mimeType": mime_type, "data": data}})
}

/// Convert the transcript to Gemini `Content[]`. The system prompt travels separately.
pub(crate) fn convert_messages(model: &Model, context: &Context) -> Vec<Value> {
    let needs_ids = requires_tool_call_id(&model.id);
    let normalize =
        |id: &str, _: &AssistantMessage| if needs_ids { take_chars(&sanitize_id(id), 64) } else { id.to_string() };
    let mut contents: Vec<Value> = Vec::new();
    for message in transform_messages(&context.messages, model, &normalize) {
        match &message {
            Message::User(user) => {
                let parts: Vec<Value> = match &user.content {
                    UserContent::Text(text) => vec![json!({"text": text})],
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text, .. } => Some(json!({"text": text})),
                            ContentBlock::Image { data, mime_type } => Some(inline_data(mime_type, data)),
                            _ => None,
                        })
                        .collect(),
                };
                if !parts.is_empty() {
                    contents.push(json!({"role": "user", "parts": parts}));
                }
            }
            Message::Assistant(assistant) => {
                let parts = assistant_parts(model, assistant, needs_ids);
                if !parts.is_empty() {
                    contents.push(json!({"role": "model", "parts": parts}));
                }
            }
            Message::ToolResult(result) => push_tool_result(model, result, needs_ids, &mut contents),
            Message::System(_) | Message::Custom(_) => {}
        }
    }
    contents
}

fn assistant_parts(model: &Model, assistant: &AssistantMessage, needs_ids: bool) -> Vec<Value> {
    let same_model = assistant.provider == model.provider && assistant.model == model.id;
    let mut parts = Vec::new();
    for block in &assistant.content {
        match block {
            ContentBlock::Text { text, text_signature } => {
                let signature = resolve_signature(same_model, text_signature.as_deref());
                if text.trim().is_empty() && signature.is_none() {
                    continue;
                }
                parts.push(with_signature(json!({"text": text}), signature));
            }
            ContentBlock::Thinking { thinking, thinking_signature, .. } if same_model => {
                let signature = resolve_signature(same_model, thinking_signature.as_deref());
                if thinking.trim().is_empty() && signature.is_none() {
                    continue;
                }
                parts.push(with_signature(json!({"thought": true, "text": thinking}), signature));
            }
            ContentBlock::Thinking { thinking, .. } => {
                if !thinking.trim().is_empty() {
                    parts.push(json!({"text": thinking}));
                }
            }
            ContentBlock::ToolCall(call) => {
                let mut function_call = json!({"name": call.name, "args": Value::Object(call.arguments.clone())});
                if needs_ids {
                    function_call["id"] = json!(call.id);
                }
                let signature = resolve_signature(same_model, call.thought_signature.as_deref());
                parts.push(with_signature(json!({"functionCall": function_call}), signature));
            }
            ContentBlock::Image { .. } => {}
        }
    }
    parts
}

fn push_tool_result(model: &Model, result: &ToolResultMessage, needs_ids: bool, contents: &mut Vec<Value>) {
    let text: Vec<&str> = result.content.iter().filter_map(ContentBlock::as_text).collect();
    let text = text.join("\n");
    let images: Vec<Value> = if model.supports_images() {
        let images = result.content.iter().filter_map(|block| match block {
            ContentBlock::Image { data, mime_type } => Some(inline_data(mime_type, data)),
            _ => None,
        });
        images.collect()
    } else {
        Vec::new()
    };
    let has_images = !images.is_empty();
    let nested_images = supports_multimodal_function_response(&model.id);
    let value = if !text.trim().is_empty() {
        text
    } else if has_images {
        "(see attached image)".to_string()
    } else {
        String::new()
    };
    let key = if result.is_error { "error" } else { "output" };
    let mut response = json!({"name": result.tool_name, "response": {key: value}});
    if has_images && nested_images {
        response["parts"] = Value::Array(images.clone());
    }
    if needs_ids {
        response["id"] = json!(result.tool_call_id);
    }
    let part = json!({"functionResponse": response});
    let merges = contents.last().is_some_and(|last| {
        last["role"] == "user"
            && last["parts"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part.get("functionResponse").is_some()))
    });
    match contents.last_mut().and_then(|last| last["parts"].as_array_mut()) {
        Some(parts) if merges => parts.push(part),
        _ => contents.push(json!({"role": "user", "parts": [part]})),
    }
    if has_images && !nested_images {
        let mut parts = vec![json!({"text": "Tool result image:"})];
        parts.extend(images);
        contents.push(json!({"role": "user", "parts": parts}));
    }
}

/// Drop JSON Schema meta declarations, recursing through objects only.
fn sanitize_for_open_api(schema: &Value) -> Value {
    match schema {
        Value::Object(object) => {
            let entries = object.iter().filter(|(key, _)| !JSON_SCHEMA_META_KEYS.contains(&key.as_str()));
            Value::Object(entries.map(|(key, value)| (key.clone(), sanitize_for_open_api(value))).collect())
        }
        other => other.clone(),
    }
}

/// Schema keywords whose values map names to subschemas, so their keys are not schema keywords.
const JSON_SCHEMA_NAME_MAPS: [&str; 6] =
    ["properties", "patternProperties", "$defs", "definitions", "dependentSchemas", "dependencies"];

/// Keywords containing a subschema or an array of subschemas, not literal data.
const JSON_SCHEMA_SUBSCHEMAS: [&str; 16] = [
    "additionalProperties",
    "unevaluatedProperties",
    "propertyNames",
    "items",
    "prefixItems",
    "additionalItems",
    "unevaluatedItems",
    "contains",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "contentSchema",
];

/// Rewrite string and number `const` values to single-value `enum`s.
/// Gemini ignores `const` in `parametersJsonSchema` but honors `enum`.
fn rewrite_const_as_enum(schema: &Value, is_name_map: bool) -> Value {
    let object = match schema {
        Value::Array(items) => {
            return Value::Array(items.iter().map(|item| rewrite_const_as_enum(item, false)).collect());
        }
        Value::Object(object) => object,
        other => return other.clone(),
    };
    let mut result: Map<String, Value> = object
        .iter()
        .map(|(key, value)| {
            let value = if is_name_map || JSON_SCHEMA_SUBSCHEMAS.contains(&key.as_str()) {
                rewrite_const_as_enum(value, false)
            } else if JSON_SCHEMA_NAME_MAPS.contains(&key.as_str()) {
                rewrite_const_as_enum(value, true)
            } else {
                value.clone()
            };
            (key.clone(), value)
        })
        .collect();
    if !is_name_map
        && !result.contains_key("enum")
        && result.get("const").is_some_and(|value| value.is_string() || value.is_number())
        && let Some(value) = result.shift_remove("const")
    {
        result.insert("enum".into(), json!([value]));
    }
    Value::Object(result)
}

/// Gemini function declarations. `use_parameters` selects the OpenAPI `parameters` field
/// instead of full JSON Schema in `parametersJsonSchema`, where `const` is sent as `enum`.
pub(crate) fn convert_tools(
    tools: &[Tool],
    use_parameters: bool,
    supports_strict: bool,
) -> Result<Option<Value>, String> {
    if tools.is_empty() {
        return Ok(None);
    }
    let mut declarations = Vec::with_capacity(tools.len());
    for tool in tools {
        let strict = resolve_json_schema_strict_sampling(tool, supports_strict, None)?;
        let parameters = json_schema_tool_parameters(tool, strict == Some(true));
        let mut declaration = Map::new();
        declaration.insert("name".into(), json!(tool.name));
        declaration.insert("description".into(), json!(tool.description));
        if use_parameters {
            declaration.insert("parameters".into(), sanitize_for_open_api(&parameters));
        } else {
            declaration.insert("parametersJsonSchema".into(), rewrite_const_as_enum(&parameters, false));
        }
        declarations.push(Value::Object(declaration));
    }
    Ok(Some(json!([{"functionDeclarations": declarations}])))
}

fn map_tool_choice(choice: &str) -> &'static str {
    match choice {
        "none" => "NONE",
        "any" => "ANY",
        _ => "AUTO",
    }
}

/// `functionCallingConfig.mode` for the request, if any.
pub(crate) fn function_calling_mode(
    tools: &[Tool],
    tool_choice: Option<&str>,
    supports_strict: bool,
) -> Result<Option<&'static str>, String> {
    let mut strict = false;
    for tool in tools {
        if resolve_json_schema_strict_sampling(tool, supports_strict, None)? == Some(true) {
            strict = true;
            break;
        }
    }
    Ok(match tool_choice {
        Some(choice @ ("none" | "any")) => Some(map_tool_choice(choice)),
        _ if strict => Some("VALIDATED"),
        choice => choice.map(map_tool_choice),
    })
}
