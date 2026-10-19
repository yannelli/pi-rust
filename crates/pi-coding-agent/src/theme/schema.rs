//! Theme JSON documents and their validation, ported from upstream `theme-schema.ts`.
//! Error messages follow the TypeBox output upstream reports, including its cap of 8 errors.

use std::collections::HashMap;

use pi_tui::colors::ColorValue;
use serde_json::{Map, Value};

use super::ThemeError;
use super::terminal::TerminalTheme;
use super::tokens::{THEME_TOKENS, ThemeToken, TokenMap};

/// The published schema URL that built-in themes reference.
pub const THEME_SCHEMA_URL: &str = "https://pi.dev/schemas/theme.schema.json";

/// The `export` section: HTML export backgrounds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThemeExportJson {
    pub page_bg: Option<ColorValue>,
    pub card_bg: Option<ColorValue>,
    pub info_bg: Option<ColorValue>,
}

/// A validated theme document (upstream `ThemeJson`).
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeJson {
    pub schema: Option<String>,
    pub name: String,
    pub appearance: Option<TerminalTheme>,
    pub vars: HashMap<String, ColorValue>,
    /// Color values by token; optional tokens may be absent.
    pub colors: TokenMap<Option<ColorValue>>,
    pub export: Option<ThemeExportJson>,
}

const MAX_ERRORS: usize = 8;
const TOP_LEVEL_KEYS: [&str; 6] = ["$schema", "name", "appearance", "vars", "colors", "export"];
const EXPORT_KEYS: [&str; 3] = ["pageBg", "cardBg", "infoBg"];

struct Errors {
    missing_colors: Vec<String>,
    other: Vec<String>,
    count: usize,
}

impl Errors {
    fn push(&mut self, path: &str, message: impl Into<String>) {
        if self.count < MAX_ERRORS {
            let path = if path.is_empty() { "/" } else { path };
            self.other.push(format!("  - {path}: {}", message.into()));
        }
        self.count += 1;
    }

    fn missing(&mut self, tokens: Vec<String>) {
        if self.count < MAX_ERRORS {
            self.missing_colors.extend(tokens);
        }
        self.count += 1;
    }
}

fn check_color_value(path: &str, value: &Value, errors: &mut Errors) {
    if value.is_string() {
        return;
    }
    let integer = value.as_f64().filter(|number| number.fract() == 0.0);
    if integer.is_some_and(|number| (0.0..=255.0).contains(&number)) {
        return;
    }
    errors.push(path, "must be string");
    match integer {
        Some(number) if number < 0.0 => errors.push(path, "must be >= 0"),
        Some(_) => errors.push(path, "must be <= 255"),
        None => errors.push(path, "must be integer"),
    }
    errors.push(path, "must match a schema in anyOf");
}

fn check_additional(path: &str, object: &Map<String, Value>, allowed: &[&str], errors: &mut Errors) {
    let extra: Vec<&String> = object.keys().filter(|key| !allowed.contains(&key.as_str())).collect();
    for key in &extra {
        errors.push(&format!("{path}/{key}"), "schema is false");
    }
    if !extra.is_empty() {
        errors.push(path, "must not have additional properties");
    }
}

fn check_object<'a>(path: &str, value: &'a Value, errors: &mut Errors) -> Option<&'a Map<String, Value>> {
    let object = value.as_object();
    if object.is_none() {
        errors.push(path, "must be object");
    }
    object
}

fn check_document(json: &Value, errors: &mut Errors) {
    let Some(root) = check_object("", json, errors) else { return };
    let missing: Vec<&str> = ["name", "colors"].into_iter().filter(|key| !root.contains_key(*key)).collect();
    if !missing.is_empty() {
        errors.push("", format!("must have required properties {}", missing.join(", ")));
    }
    check_additional("", root, &TOP_LEVEL_KEYS, errors);
    if root.get("$schema").is_some_and(|value| !value.is_string()) {
        errors.push("/$schema", "must be string");
    }
    match root.get("name") {
        Some(Value::String(name)) if name.is_empty() || name.contains('/') => {
            errors.push("/name", "must match pattern \"^[^/]+$\"")
        }
        Some(value) if !value.is_string() => errors.push("/name", "must be string"),
        _ => {}
    }
    match root.get("appearance") {
        Some(Value::String(text)) if text == "dark" || text == "light" => {}
        Some(value) => {
            for _ in 0..2 {
                if !value.is_string() {
                    errors.push("/appearance", "must be string");
                }
                errors.push("/appearance", "must be equal to constant");
            }
            errors.push("/appearance", "must match a schema in anyOf");
        }
        None => {}
    }
    if let Some(vars) = root.get("vars").and_then(|value| check_object("/vars", value, errors)) {
        for (key, value) in vars {
            check_color_value(&format!("/vars/{key}"), value, errors);
        }
    }
    if let Some(colors) = root.get("colors").and_then(|value| check_object("/colors", value, errors)) {
        let missing: Vec<String> = THEME_TOKENS
            .iter()
            .filter(|descriptor| descriptor.fallback.is_none() && !colors.contains_key(descriptor.name))
            .map(|descriptor| descriptor.name.to_string())
            .collect();
        if !missing.is_empty() {
            errors.missing(missing);
        }
        let names: Vec<&str> = THEME_TOKENS.iter().map(|descriptor| descriptor.name).collect();
        check_additional("/colors", colors, &names, errors);
        for descriptor in THEME_TOKENS {
            if let Some(value) = colors.get(descriptor.name) {
                check_color_value(&format!("/colors/{}", descriptor.name), value, errors);
            }
        }
    }
    if let Some(export) = root.get("export").and_then(|value| check_object("/export", value, errors)) {
        check_additional("/export", export, &EXPORT_KEYS, errors);
        for key in EXPORT_KEYS {
            if let Some(value) = export.get(key) {
                check_color_value(&format!("/export/{key}"), value, errors);
            }
        }
    }
}

/// The upstream error for names containing `/`, which is reserved for `light/dark` settings.
pub fn invalid_theme_name_error(name: &str) -> ThemeError {
    ThemeError(format!(
        "Invalid theme name \"{name}\": theme names cannot contain \"/\" because it is reserved for automatic \
         light/dark theme settings."
    ))
}

/// Validate one theme document, failing with a message that names the offending tokens.
pub fn validate_theme_json(label: &str, json: &Value) -> Result<ThemeJson, ThemeError> {
    if let Some(name) = json.get("name").and_then(Value::as_str).filter(|name| name.contains('/')) {
        return Err(invalid_theme_name_error(name));
    }
    let mut errors = Errors { missing_colors: Vec::new(), other: Vec::new(), count: 0 };
    check_document(json, &mut errors);
    if errors.count == 0 {
        return Ok(theme_json_from_value(json));
    }
    let mut message = format!("Invalid theme \"{label}\":\n");
    if !errors.missing_colors.is_empty() {
        errors.missing_colors.sort();
        message.push_str("\nMissing required color tokens:\n");
        let lines: Vec<String> = errors.missing_colors.iter().map(|color| format!("  - {color}")).collect();
        message.push_str(&lines.join("\n"));
        message.push_str("\n\nPlease add these colors to your theme's \"colors\" object.");
        message.push_str("\nSee the built-in themes (dark.json, light.json) for reference values.");
    }
    if !errors.other.is_empty() {
        message.push_str(&format!("\n\nOther errors:\n{}", errors.other.join("\n")));
    }
    Err(ThemeError(message))
}

fn color_value(value: &Value) -> Option<ColorValue> {
    match value {
        Value::String(text) => Some(ColorValue::Text(text.clone())),
        Value::Number(number) => number.as_f64().map(ColorValue::Number),
        _ => None,
    }
}

/// Read a document that already passed validation (or a built-in theme) without checking it again.
pub(crate) fn theme_json_from_value(json: &Value) -> ThemeJson {
    let field = |key: &str| json.get(key);
    let vars = field("vars")
        .and_then(Value::as_object)
        .map(|vars| vars.iter().filter_map(|(key, value)| Some((key.clone(), color_value(value)?))).collect())
        .unwrap_or_default();
    let colors = field("colors").and_then(Value::as_object);
    let colors =
        TokenMap::from_fn(|token: ThemeToken| colors.and_then(|map| map.get(token.name())).and_then(color_value));
    let export = field("export").and_then(Value::as_object).map(|export| {
        let get = |key: &str| export.get(key).and_then(color_value);
        ThemeExportJson { page_bg: get("pageBg"), card_bg: get("cardBg"), info_bg: get("infoBg") }
    });
    ThemeJson {
        schema: field("$schema").and_then(Value::as_str).map(String::from),
        name: field("name").and_then(Value::as_str).unwrap_or_default().to_string(),
        appearance: match field("appearance").and_then(Value::as_str) {
            Some("dark") => Some(TerminalTheme::Dark),
            Some("light") => Some(TerminalTheme::Light),
            _ => None,
        },
        vars,
        colors,
        export,
    }
}

/// Parse theme JSON text (a leading BOM is ignored) and validate it.
pub fn parse_theme_json_content(label: &str, content: &str) -> Result<ThemeJson, ThemeError> {
    let json: Value = serde_json::from_str(content.trim_start_matches('\u{feff}'))
        .map_err(|error| ThemeError(format!("Failed to parse theme {label}: {error}")))?;
    validate_theme_json(label, &json)
}

/// The theme JSON Schema document, generated from the token table like upstream's published schema.
pub fn theme_json_schema() -> Value {
    let color_value = |description: Option<&str>| {
        let mut schema = serde_json::json!({ "anyOf": [
            { "type": "string", "description": "Hex color (#RGB or #RRGGBB), OKLCH or OKHSL color, variable reference, or empty string for terminal default" },
            { "type": "integer", "minimum": 0, "maximum": 255, "description": "256-color palette index (0-255)" },
        ]});
        if let Some(description) = description {
            schema["description"] = Value::String(description.to_string());
        }
        schema
    };
    let mut properties = Map::new();
    let mut required = Vec::new();
    for descriptor in THEME_TOKENS {
        properties.insert(descriptor.name.to_string(), color_value(Some(descriptor.description)));
        if descriptor.fallback.is_none() {
            required.push(Value::String(descriptor.name.to_string()));
        }
    }
    serde_json::json!({
        "title": "Pi Coding Agent Theme",
        "description": "Theme schema for Pi coding agent",
        "type": "object",
        "additionalProperties": false,
        "required": ["name", "colors"],
        "properties": {
            "$schema": { "type": "string", "description": "JSON schema reference" },
            "name": { "type": "string", "pattern": "^[^/]+$", "description": "Theme name. Must not contain '/' because it is reserved for automatic light/dark theme settings." },
            "appearance": { "anyOf": [{ "const": "dark", "type": "string" }, { "const": "light", "type": "string" }], "description": "Background the theme is designed for. Detected from the theme colors when omitted." },
            "vars": { "type": "object", "additionalProperties": color_value(None), "description": "Reusable color variables" },
            "colors": {
                "type": "object",
                "description": "Theme color definitions (scrollbar, thinkingMax, and search highlight colors are optional and use compatible fallbacks)",
                "additionalProperties": false,
                "required": required,
                "properties": properties,
            },
            "export": {
                "type": "object",
                "description": "Optional colors for HTML export (defaults derived from userMessageBg if not specified)",
                "additionalProperties": false,
                "properties": {
                    "pageBg": color_value(Some("Page background color")),
                    "cardBg": color_value(Some("Card/container background color")),
                    "infoBg": color_value(Some("Info sections background (system prompt, notices)")),
                },
            },
        },
    })
}
