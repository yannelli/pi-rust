//! YAML frontmatter, ported from upstream `utils/frontmatter.ts`.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

static LOCATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" at line ([0-9]+) column ([0-9]+)").unwrap());

/// Frontmatter fields and the trimmed body after them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedFrontmatter {
    pub frontmatter: Map<String, Value>,
    pub body: String,
}

/// A YAML error with its location, such as `... at line 1, column 14`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontmatterError(pub String);

impl std::fmt::Display for FrontmatterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for FrontmatterError {}

fn normalize_newlines(text: &str) -> String {
    text.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n")
}

/// Split `---` delimited YAML from the body. Content without frontmatter keeps its body untrimmed.
pub fn parse_frontmatter(content: &str) -> Result<ParsedFrontmatter, FrontmatterError> {
    let normalized = normalize_newlines(content);
    let end = normalized.strip_prefix("---").and_then(|rest| rest.find("\n---")).map(|at| at + 3);
    let Some(end) = end else {
        return Ok(ParsedFrontmatter { frontmatter: Map::new(), body: normalized });
    };
    let yaml = normalized.get(4..end).unwrap_or_default();
    let body = normalized[end + 4..].trim().to_string();
    if yaml.is_empty() {
        return Ok(ParsedFrontmatter { frontmatter: Map::new(), body });
    }
    let value: Value = serde_yaml_ng::from_str(yaml).map_err(|error| {
        let message = error.to_string();
        let message = LOCATION.replace_all(&message, " at line $1, column $2").into_owned();
        match error.location() {
            Some(at) if !message.contains(" at line ") => {
                FrontmatterError(format!("{message} at line {}, column {}", at.line(), at.column()))
            }
            _ => FrontmatterError(message),
        }
    })?;
    let frontmatter = match value {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    Ok(ParsedFrontmatter { frontmatter, body })
}

/// The body without frontmatter; content with malformed frontmatter is returned unchanged.
pub fn strip_frontmatter(content: &str) -> String {
    parse_frontmatter(content).map(|parsed| parsed.body).unwrap_or_else(|_| content.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fields_and_trims_body() {
        let parsed =
            parse_frontmatter("\u{feff}---\r\nname: a\r\ndescription: |\r\n  x\r\n  y\r\n---\r\n\r\nBody\r\n").unwrap();
        assert_eq!(parsed.frontmatter["name"], "a");
        assert!(parsed.frontmatter["description"].as_str().unwrap().starts_with("x\ny"));
        assert_eq!(parsed.body, "Body");
    }

    #[test]
    fn content_without_frontmatter_keeps_body() {
        let parsed = parse_frontmatter("  plain\n").unwrap();
        assert!(parsed.frontmatter.is_empty());
        assert_eq!(parsed.body, "  plain\n");
        assert_eq!(parse_frontmatter("---\nunterminated").unwrap().body, "---\nunterminated");
        assert!(parse_frontmatter("---\n---\nbody").unwrap().frontmatter.is_empty());
    }

    #[test]
    fn reports_yaml_errors_with_location() {
        let error = parse_frontmatter("---\ndescription: Broken: unquoted colon\n---\nDo it.\n").unwrap_err();
        assert!(error.0.contains("line 1"), "{error}");
        assert!(error.0.contains("line 1, column "), "{error}");
    }
}
