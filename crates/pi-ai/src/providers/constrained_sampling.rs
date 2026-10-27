//! Tool constrained sampling: strict JSON-schema tools and OpenAI grammar tools.
//! Port of upstream `api/constrained-sampling.ts` plus the Responses tool conversion.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::types::{JsonObject, Tool};

/// Schema keywords that strict provider modes reject outright.
const UNSUPPORTED_STRICT_SCHEMA_KEYS: [&str; 16] = [
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

/// Returns true when a provider's strict mode rejects this schema keyword with this value.
pub type UnsupportedKeywordCheck<'a> = &'a dyn Fn(&str, &Value) -> bool;

/// A schema that cannot be converted to the strict subset; the text names the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsupportedStrictSchema(pub String);

impl std::fmt::Display for UnsupportedStrictSchema {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn unsupported<T>(message: impl Into<String>) -> Result<T, UnsupportedStrictSchema> {
    Err(UnsupportedStrictSchema(message.into()))
}

fn type_names(schema: &Map<String, Value>) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn is_structured_schema(schema: &Value) -> bool {
    let Some(schema) = schema.as_object() else { return false };
    let types = type_names(schema);
    types.contains(&"object")
        || types.contains(&"array")
        || schema.contains_key("properties")
        || schema.contains_key("items")
}

fn schema_allows_null(schema: &Value) -> bool {
    let Some(schema) = schema.as_object() else { return false };
    if type_names(schema).contains(&"null") {
        return true;
    }
    if schema.get("const") == Some(&Value::Null)
        || schema.get("enum").and_then(Value::as_array).is_some_and(|values| values.contains(&Value::Null))
    {
        return true;
    }
    schema.get("anyOf").and_then(Value::as_array).is_some_and(|variants| variants.iter().any(schema_allows_null))
}

fn make_node_strict(schema: &mut Value, check: Option<UnsupportedKeywordCheck>) -> Result<(), UnsupportedStrictSchema> {
    let Some(node) = schema.as_object_mut() else { return unsupported("boolean schemas are unsupported") };
    for key in UNSUPPORTED_STRICT_SCHEMA_KEYS {
        if node.contains_key(key) {
            return unsupported(format!("{key} schemas are unsupported"));
        }
    }
    if let Some(check) = check
        && let Some((key, value)) = node.iter().find(|(key, value)| check(key, value))
    {
        return unsupported(format!("{key}: {value} is unsupported"));
    }
    if let Some(any_of) = node.get_mut("anyOf") {
        let Some(variants) = any_of.as_array_mut().filter(|variants| !variants.is_empty()) else {
            return unsupported("anyOf must contain at least one schema");
        };
        for variant in variants {
            if is_structured_schema(variant) {
                return unsupported("object and array unions are unsupported");
            }
            make_node_strict(variant, check)?;
        }
    }
    if let Some(items) = node.get_mut("items") {
        if items.is_array() {
            return unsupported("tuple schemas are unsupported");
        }
        make_node_strict(items, check)?;
    }
    let is_object = node.get("type").and_then(Value::as_str) == Some("object");
    if node.contains_key("properties") && !is_object {
        return unsupported("properties require type object");
    }
    if !is_object {
        return Ok(());
    }
    if node.get("additionalProperties").is_some_and(|value| value != &Value::Bool(false)) {
        return unsupported("schema-valued or true additionalProperties is unsupported");
    }
    if node.get("properties").is_some_and(|properties| !properties.is_object()) {
        return unsupported("object properties must be a schema map");
    }
    let required: Vec<String> = match node.get("required") {
        None => Vec::new(),
        Some(Value::Array(keys)) if keys.iter().all(Value::is_string) => {
            keys.iter().filter_map(Value::as_str).map(str::to_string).collect()
        }
        Some(_) => return unsupported("object required must be a string array"),
    };
    let mut names = Vec::new();
    if let Some(Value::Object(properties)) = node.get_mut("properties") {
        if required.iter().any(|key| !properties.contains_key(key)) {
            return unsupported("required contains an unknown property");
        }
        for (key, property) in properties.iter_mut() {
            make_node_strict(property, check)?;
            if !required.contains(key) && !schema_allows_null(property) {
                *property = json!({"anyOf": [property.take(), {"type": "null"}]});
            }
            names.push(Value::String(key.clone()));
        }
    } else if !required.is_empty() {
        return unsupported("required contains an unknown property");
    }
    node.insert("required".into(), Value::Array(names));
    node.insert("additionalProperties".into(), Value::Bool(false));
    Ok(())
}

/// Convert a tool schema to the strict subset expected by provider constrained sampling.
pub fn make_strict_json_schema(
    schema: &Value,
    check: Option<UnsupportedKeywordCheck>,
) -> Result<Value, UnsupportedStrictSchema> {
    let mut cloned = schema.clone();
    if !cloned.is_object() {
        return unsupported("root schema must have type object");
    }
    make_node_strict(&mut cloned, check)?;
    if cloned.get("type").and_then(Value::as_str) != Some("object") {
        return unsupported("root schema must have type object");
    }
    Ok(cloned)
}

/// Tool parameters as sent: the strict schema when `strict` is true, else the original.
pub fn json_schema_tool_parameters(tool: &Tool, strict: bool) -> Value {
    if strict && let Ok(schema) = make_strict_json_schema(&tool.parameters, None) {
        return schema;
    }
    tool.parameters.clone()
}

fn config_type(tool: &Tool) -> Option<&str> {
    tool.constrained_sampling.as_ref()?.get("type")?.as_str()
}

/// Whether a JSON-schema tool is sent in strict mode: `Some(true)` for strict, `None` to
/// leave the provider default. Errors when the tool requires strict mode and cannot have it.
pub fn resolve_json_schema_strict_sampling(
    tool: &Tool,
    supports_strict_mode: bool,
    check: Option<UnsupportedKeywordCheck>,
) -> Result<Option<bool>, String> {
    if config_type(tool) != Some("json_schema") {
        return Ok(None);
    }
    let required =
        tool.constrained_sampling.as_ref().and_then(|config| config.get("strict")?.as_str()) == Some("require");
    if supports_strict_mode {
        return match make_strict_json_schema(&tool.parameters, check) {
            Ok(_) => Ok(Some(true)),
            Err(_) if !required => Ok(None),
            Err(error) => {
                Err(format!("Tool \"{}\" requires JSON-schema constrained sampling, but {error}.", tool.name))
            }
        };
    }
    if required {
        return Err(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        ));
    }
    Ok(None)
}

/// OpenAI grammar syntax of a grammar tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrammarFormat {
    Lark,
    Regex,
}

impl GrammarFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lark => "lark",
            Self::Regex => "regex",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrammarConstrainedSampling {
    pub format: GrammarFormat,
    pub definition: String,
    /// The single required string property that carries the raw grammar input.
    pub input_property: String,
}

/// Streaming state for re-encoding raw grammar input as an append-only JSON object.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrammarToolInputJsonBuffer {
    pub input: String,
    pub started: bool,
    pub closed: bool,
}

/// The raw input of a grammar tool call, read from its string argument.
pub fn grammar_tool_input(tool_name: &str, arguments: &JsonObject, input_property: &str) -> Result<String, String> {
    match arguments.get(input_property) {
        Some(Value::String(input)) => Ok(input.clone()),
        _ => Err(format!("Grammar tool call \"{tool_name}\" requires argument \"{input_property}\" to be a string.")),
    }
}

/// Append `input_delta` to a streaming grammar tool input and return the matching piece of its JSON
/// arguments text, or `None` for an empty delta. Costs time linear in the delta, never in the input.
pub fn append_grammar_tool_input_json_delta(
    buffer: &mut GrammarToolInputJsonBuffer,
    input_property: &str,
    input_delta: &str,
) -> Result<Option<String>, String> {
    if buffer.closed {
        return Err(format!("grammar tool input for property \"{input_property}\" changed after it was closed"));
    }
    if input_delta.is_empty() {
        return Ok(None);
    }
    buffer.input.push_str(input_delta);
    Ok(Some(grammar_tool_input_json_start(buffer, input_property) + &json_string_body(input_delta)))
}

/// Close a streaming grammar tool input and return the rest of its JSON arguments text. `final_input`
/// is the complete input when the provider sends one; it must continue the input streamed so far.
/// Closing again with the same input returns `None`.
pub fn close_grammar_tool_input_json(
    buffer: &mut GrammarToolInputJsonBuffer,
    input_property: &str,
    final_input: Option<&str>,
) -> Result<Option<String>, String> {
    if buffer.closed {
        if final_input.is_none_or(|input| input == buffer.input) {
            return Ok(None);
        }
        return Err(format!("grammar tool input for property \"{input_property}\" changed after it was closed"));
    }
    let rest = match final_input {
        Some(input) => match input.strip_prefix(buffer.input.as_str()) {
            Some(rest) => rest.to_string(),
            None => {
                return Err(format!("grammar tool input for property \"{input_property}\" changed non-monotonically"));
            }
        },
        None => String::new(),
    };
    buffer.input.push_str(&rest);
    buffer.closed = true;
    Ok(Some(grammar_tool_input_json_start(buffer, input_property) + &json_string_body(&rest) + "\"}"))
}

/// The opening of the JSON arguments text, once.
fn grammar_tool_input_json_start(buffer: &mut GrammarToolInputJsonBuffer, input_property: &str) -> String {
    if buffer.started {
        return String::new();
    }
    buffer.started = true;
    format!("{{{}:\"", Value::String(input_property.to_string()))
}

/// `text` encoded as the inside of a JSON string literal.
fn json_string_body(text: &str) -> String {
    let encoded = Value::String(text.to_string()).to_string();
    encoded[1..encoded.len() - 1].to_string()
}

fn infer_grammar_input_property(tool: &Tool) -> Result<String, String> {
    let schema = &tool.parameters;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err("grammar constrained sampling requires an object parameter schema".into());
    }
    let required = schema.get("required").and_then(Value::as_array);
    let Some(property) = required.filter(|required| required.len() == 1).and_then(|required| required[0].as_str())
    else {
        return Err("grammar constrained sampling requires exactly one required string property".into());
    };
    let Some(definition) = schema.pointer(&format!("/properties/{}", property.replace('~', "~0").replace('/', "~1")))
    else {
        return Err(format!("grammar constrained sampling requires a properties entry for {property}"));
    };
    if definition.get("type").and_then(Value::as_str) != Some("string") {
        return Err(format!("grammar constrained sampling property {property} must have type string"));
    }
    Ok(property.to_string())
}

/// The grammar to send for a grammar tool, or `None` when the tool has no grammar
/// config or the provider lacks grammar tools.
pub fn resolve_grammar_constrained_sampling(
    tool: &Tool,
    supports_openai_grammar_tools: bool,
) -> Result<Option<GrammarConstrainedSampling>, String> {
    if config_type(tool) != Some("grammar") || !supports_openai_grammar_tools {
        return Ok(None);
    }
    let variants = tool.constrained_sampling.as_ref().and_then(|config| config.get("variants"));
    let variant = |name: &str| {
        variants.and_then(|variants| variants.get(name)?.as_str()).filter(|definition| !definition.trim().is_empty())
    };
    let (format, definition) = match (variant("openai_lark"), variant("openai_regex")) {
        (Some(lark), _) => (GrammarFormat::Lark, lark),
        (None, Some(regex)) => (GrammarFormat::Regex, regex),
        (None, None) => {
            return Err(format!(
                "Tool \"{}\" cannot use grammar constrained sampling: no supported grammar variant was provided.",
                tool.name
            ));
        }
    };
    let input_property = infer_grammar_input_property(tool)
        .map_err(|error| format!("Tool \"{}\" cannot use grammar constrained sampling: {error}.", tool.name))?;
    Ok(Some(GrammarConstrainedSampling { format, definition: definition.to_string(), input_property }))
}

/// Tool name to grammar input property, for every tool sent as a grammar tool.
pub fn grammar_tool_input_properties(
    tools: &[Tool],
    supports_openai_grammar_tools: bool,
) -> Result<HashMap<String, String>, String> {
    let mut properties = HashMap::new();
    for tool in tools {
        if let Some(grammar) = resolve_grammar_constrained_sampling(tool, supports_openai_grammar_tools)? {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    Ok(properties)
}

/// Options for [`convert_responses_tools`], from upstream `ConvertResponsesToolsOptions`.
#[derive(Clone, Copy, Debug)]
pub struct ResponsesToolOptions {
    /// Default `strict` value; `None` sends `null`.
    pub strict: Option<bool>,
    pub supports_strict_mode: bool,
    pub supports_openai_grammar_tools: bool,
    pub tool_search_result: bool,
}

impl Default for ResponsesToolOptions {
    fn default() -> Self {
        Self {
            strict: Some(false),
            supports_strict_mode: true,
            supports_openai_grammar_tools: false,
            tool_search_result: false,
        }
    }
}

/// OpenAI Responses tool declarations: grammar tools become `custom` tools.
pub fn convert_responses_tools(tools: &[Tool], options: ResponsesToolOptions) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .map(|tool| {
            let mut value = match resolve_grammar_constrained_sampling(tool, options.supports_openai_grammar_tools)? {
                Some(grammar) => json!({
                    "type": "custom", "name": tool.name, "description": tool.description,
                    "format": {"type": "grammar", "syntax": grammar.format.as_str(), "definition": grammar.definition},
                }),
                None => {
                    let constrained = resolve_json_schema_strict_sampling(tool, options.supports_strict_mode, None)?;
                    let strict = constrained.or(options.strict);
                    let mut value = json!({
                        "type": "function", "name": tool.name, "description": tool.description,
                        "parameters": json_schema_tool_parameters(tool, strict == Some(true)),
                    });
                    if options.supports_strict_mode {
                        value["strict"] = strict.map_or(Value::Null, Value::Bool);
                    }
                    value
                }
            };
            if options.tool_search_result {
                value["defer_loading"] = json!(true);
            }
            Ok(value)
        })
        .collect()
}

#[cfg(test)]
#[path = "constrained_sampling_tests.rs"]
mod tests;
