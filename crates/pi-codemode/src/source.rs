//! Codemode source format: JavaScript, optionally preceded by one line such as
//! `// @options: {"max_output_tokens": 2000, "timeout_ms": 30000}`.

use std::fmt;

use serde_json::Value;

use crate::js::js_trim;

pub const CODEMODE_OPTIONS_PREFIX: &str = "// @options:";

const SUPPORTED_FIELDS: [&str; 2] = ["max_output_tokens", "timeout_ms"];
const SUPPORTED_FIELDS_TEXT: &str = "`max_output_tokens` and `timeout_ms`";
/// Largest delay `setTimeout` supports upstream, which bounds `timeout_ms`.
pub const MAX_TIMEOUT_MS: u64 = 2_147_483_647;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Lark grammar for providers with grammar-constrained tool input. It only fixes the shape of the
/// options line; the options JSON and the code are checked by [`parse_codemode_source`].
pub const CODEMODE_SOURCE_GRAMMAR: &str = r#"
start: options_source | plain_source
options_source: OPTIONS_LINE NEWLINE SOURCE
plain_source: SOURCE

OPTIONS_LINE: /[ \t]*\/\/ @options:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodemodeSourceOptions {
    /// Token budget for the script's output.
    pub max_output_tokens: Option<u64>,
    /// Hard deadline for the whole script in milliseconds, including tool calls.
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCodemodeSource {
    /// The script with the options line replaced by an empty line, so line numbers are unchanged.
    pub code: String,
    pub options: CodemodeSourceOptions,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodemodeSourceError(pub String);

impl fmt::Display for CodemodeSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CodemodeSourceError {}

fn fail<T>(message: impl Into<String>) -> Result<T, CodemodeSourceError> {
    Err(CodemodeSourceError(message.into()))
}

fn safe_integer(value: &Value) -> Option<u64> {
    let number = value.as_f64()?;
    (number.fract() == 0.0 && (0.0..=MAX_SAFE_INTEGER).contains(&number)).then_some(number as u64)
}

fn parse_options(directive: &str) -> Result<CodemodeSourceOptions, CodemodeSourceError> {
    let not_object = || fail(format!("@options must be a JSON object with supported fields {SUPPORTED_FIELDS_TEXT}"));
    if directive.is_empty() {
        return not_object();
    }
    let value: Value = match serde_json::from_str(directive) {
        Ok(value) => value,
        Err(error) => {
            return fail(format!("@options must be valid JSON with supported fields {SUPPORTED_FIELDS_TEXT}: {error}"));
        }
    };
    let Value::Object(fields) = value else {
        return not_object();
    };
    if let Some(key) = fields.keys().find(|key| !SUPPORTED_FIELDS.contains(&key.as_str())) {
        return fail(format!("@options only supports {SUPPORTED_FIELDS_TEXT}; got `{key}`"));
    }
    let mut options = CodemodeSourceOptions::default();
    if let Some(value) = fields.get("max_output_tokens") {
        let Some(tokens) = safe_integer(value) else {
            return fail("@options field `max_output_tokens` must be a non-negative safe integer");
        };
        options.max_output_tokens = Some(tokens);
    }
    if let Some(value) = fields.get("timeout_ms") {
        match safe_integer(value) {
            Some(ms) if ms > 0 && ms <= MAX_TIMEOUT_MS => options.timeout_ms = Some(ms),
            _ => return fail(format!("@options field `timeout_ms` must be a positive integer up to {MAX_TIMEOUT_MS}")),
        }
    }
    Ok(options)
}

fn is_blank(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// Split an optional first-line `// @options: {...}` from the script. Fails for empty input, invalid
/// options, and an options line that is not the first line, which would otherwise be ignored
/// without notice.
pub fn parse_codemode_source(input: &str) -> Result<ParsedCodemodeSource, CodemodeSourceError> {
    if js_trim(input).is_empty() {
        return fail(
            "Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line `// @options: {\"max_output_tokens\": 1000}`.",
        );
    }
    let misplaced = input
        .split('\n')
        .enumerate()
        .skip(1)
        .find(|(_, line)| line.trim_start_matches(is_blank).starts_with(CODEMODE_OPTIONS_PREFIX));
    if let Some((index, _)) = misplaced {
        return fail(format!(
            "The {CODEMODE_OPTIONS_PREFIX} line must be the first line of the input; found one on line {}",
            index + 1
        ));
    }
    let newline = input.find('\n');
    let first_line = newline.map_or(input, |index| &input[..index]);
    let first_line = first_line.strip_suffix('\r').unwrap_or(first_line);
    let trimmed = first_line.trim_start_matches(is_blank);
    let Some(directive) = trimmed.strip_prefix(CODEMODE_OPTIONS_PREFIX) else {
        return Ok(ParsedCodemodeSource { code: input.into(), options: CodemodeSourceOptions::default() });
    };
    let code = newline.map_or("", |index| &input[index..]);
    if js_trim(code).is_empty() {
        return fail("The @options line must be followed by JavaScript source on subsequent lines");
    }
    Ok(ParsedCodemodeSource { code: code.into(), options: parse_options(js_trim(directive))? })
}
