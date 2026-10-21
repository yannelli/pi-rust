//! Parsers for the TypeScript and Rust constructs the parity sections compare. They are
//! heuristics over text, so each one returns what it can find and `None` when the anchor is gone.

use std::collections::{BTreeSet, HashSet};

use super::scan::{Lang, first_literal_per_group, group_after, is_ident_byte, matching_close, member_names};
use super::scan::{skip_lexeme, starts_with_any, top_level_literals};

/// `--long-flag` names that appear as an exact quoted string or at the start of a help line.
pub fn long_flags(src: &str) -> BTreeSet<String> {
    let mut flags = BTreeSet::new();
    for line in src.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("--") {
            insert_flag(&mut flags, rest, None);
        }
        let mut rest = line;
        while let Some(at) = rest.find("\"--").or_else(|| rest.find("'--")) {
            let quote = rest.as_bytes()[at] as char;
            insert_flag(&mut flags, &rest[at + 3..], Some(quote));
            rest = &rest[at + 3..];
        }
    }
    flags
}

fn insert_flag(flags: &mut BTreeSet<String>, text: &str, quote: Option<char>) {
    let name: String = text.chars().take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-').collect();
    let after = text[name.len()..].chars().next();
    let closed = quote.is_none() || after == quote;
    if closed && name.starts_with(|c: char| c.is_ascii_lowercase()) && !name.ends_with('-') {
        flags.insert(format!("--{name}"));
    }
}

/// The member names of the first of `anchors` found in `src`, with the anchor that matched.
pub fn ts_members(src: &str, anchors: &[&'static str]) -> Option<(&'static str, Vec<String>)> {
    anchors.iter().find_map(|anchor| {
        let at = src.find(anchor)?;
        Some((*anchor, member_names(group_after(src, at, b'{', Lang::Ts)?)))
    })
}

/// The first string literal of each entry in the array assigned to `const <name>`.
pub fn const_array_entries(src: &str, name: &str, lang: Lang) -> Option<Vec<String>> {
    let content = const_array_body(src, name, lang)?;
    Some(first_literal_per_group(content, lang))
}

/// The string literals of the array assigned to `const <name>`.
pub fn const_array_literals(src: &str, name: &str, lang: Lang) -> Option<Vec<String>> {
    Some(top_level_literals(const_array_body(src, name, lang)?, lang))
}

fn const_array_body<'a>(src: &'a str, name: &str, lang: Lang) -> Option<&'a str> {
    let declaration = src.find(&format!("const {name}"))?;
    let assign = declaration + src[declaration..].find('=')?;
    group_after(src, assign, b'[', lang)
}

/// Identifier-like words in code lines, skipping lines that start a comment.
pub fn identifiers(src: &str, into: &mut HashSet<String>) {
    for line in src.lines() {
        if starts_with_any(line.trim_start(), &["//", "/*", "*"]) {
            continue;
        }
        into.extend(
            line.split(|c: char| !(c.is_ascii() && is_ident_byte(c as u8))).filter(|w| !w.is_empty()).map(String::from),
        );
    }
}

/// The serialized field names of `pub struct <name>`, from `rename_all`, `rename`, `skip`, and `flatten`.
pub fn serde_fields(src: &str, name: &str) -> Option<Vec<String>> {
    let header = struct_header(src, name)?;
    let rule = serde_value(&attributes_above(src, header), "rename_all");
    let body = group_after(src, header, b'{', Lang::Rust)?;
    let (mut fields, mut attrs, mut i) = (Vec::new(), String::new(), 0);
    while i < body.len() {
        let rest = &body[i..];
        if rest.starts_with(char::is_whitespace) {
            i += rest.len() - rest.trim_start().len();
        } else if rest.starts_with("#[") {
            let end = matching_close(body, i + 1, Lang::Rust)?;
            attrs.push_str(&body[i + 2..end]);
            attrs.push('\n');
            i = end + 1;
        } else if let Some(end) = skip_lexeme(body, i, Lang::Rust).filter(|_| rest.starts_with("//")) {
            i = end;
        } else {
            let (field, end) = field_at(body, i)?;
            fields.extend(serialized_name(&field, &attrs, rule.as_deref()));
            attrs.clear();
            i = end;
        }
    }
    Some(fields)
}

fn struct_header(src: &str, name: &str) -> Option<usize> {
    let pattern = format!("pub struct {name}");
    src.match_indices(&pattern)
        .map(|(at, _)| at)
        .find(|at| !src.as_bytes().get(at + pattern.len()).is_some_and(|&b| is_ident_byte(b)))
}

/// The attribute and comment lines directly above the item at `at`.
fn attributes_above(src: &str, at: usize) -> String {
    let lines: Vec<&str> = src[..at].lines().collect();
    let kept = lines.iter().rev().take_while(|line| starts_with_any(line.trim_start(), &["#[", "//"])).count();
    lines[lines.len() - kept..].join("\n")
}

/// The field name and the index after its trailing comma, for the field that starts at `start`.
fn field_at(body: &str, start: usize) -> Option<(String, usize)> {
    let b = body.as_bytes();
    let colon = start + body[start..].find(':')?;
    let name = body[start..colon].split_whitespace().last()?.trim_start_matches("r#").to_string();
    let (mut depth, mut i) = (0i32, colon + 1);
    while i < b.len() {
        if let Some(end) = skip_lexeme(body, i, Lang::Rust) {
            i = end;
            continue;
        }
        match b[i] {
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b'>' if b[i - 1] != b'-' => depth -= 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth <= 0 => return Some((name, i + 1)),
            _ => {}
        }
        i += 1;
    }
    Some((name, b.len()))
}

fn serialized_name(field: &str, attrs: &str, rule: Option<&str>) -> Option<String> {
    let args = serde_args(attrs);
    if args.iter().any(|arg| arg == "skip" || arg == "flatten") {
        return None;
    }
    if let Some(renamed) = serde_value(attrs, "rename") {
        return Some(renamed);
    }
    Some(rename_by_rule(field, rule))
}

/// Comma separated arguments of every `serde(...)` in `attrs`.
fn serde_args(attrs: &str) -> Vec<String> {
    let mut args = Vec::new();
    for (at, _) in attrs.match_indices("serde(") {
        let open = at + "serde".len();
        let Some(close) = matching_close(attrs, open, Lang::Rust) else { continue };
        let content = &attrs[open + 1..close];
        let (mut depth, mut start) = (0i32, 0);
        for (index, c) in content.char_indices() {
            match c {
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                ',' if depth == 0 => {
                    args.push(content[start..index].trim().to_string());
                    start = index + 1;
                }
                _ => {}
            }
        }
        args.push(content[start..].trim().to_string());
    }
    args.retain(|arg| !arg.is_empty());
    args
}

/// The string in a `serde(<key> = "value")` argument.
fn serde_value(attrs: &str, key: &str) -> Option<String> {
    serde_args(attrs).iter().find_map(|arg| {
        let value = arg.strip_prefix(key)?.trim_start().strip_prefix('=')?;
        Some(value.trim().trim_matches('"').to_string())
    })
}

fn rename_by_rule(field: &str, rule: Option<&str>) -> String {
    let words: Vec<&str> = field.split('_').filter(|word| !word.is_empty()).collect();
    let capital = |word: &str| {
        let mut chars = word.chars();
        chars.next().map(|first| first.to_uppercase().chain(chars).collect::<String>()).unwrap_or_default()
    };
    match rule {
        Some("camelCase") => {
            words.iter().enumerate().map(|(i, word)| if i == 0 { word.to_string() } else { capital(word) }).collect()
        }
        Some("PascalCase") => words.iter().map(|word| capital(word)).collect(),
        Some("kebab-case") => words.join("-"),
        Some("SCREAMING_SNAKE_CASE" | "UPPERCASE") => field.to_uppercase(),
        Some("lowercase") => field.to_lowercase(),
        _ => field.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn flags_from_args_ts() {
        let src = r#"
		} else if (arg === "--mode") {
		} else if (arg === "--no-tools" || arg === "-nt") {
		} else if (arg === '--use-theme') {
		} else if (arg.startsWith("--")) {
		console.log(`
Options:
  --provider <name>              Provider to search
  --                             End option parsing
  ${APP_NAME} --export ~/x/--path--/session.jsonl
Extensions can register additional flags (e.g., --plan from plan-mode).
`);
"#;
        let flags: Vec<String> = long_flags(src).into_iter().collect();
        assert_eq!(flags, strings(&["--mode", "--no-tools", "--provider", "--use-theme"]));
    }

    #[test]
    fn flags_from_rust_match_arms() {
        let src = "\"-h\" | \"--help\" => a, \"--mode requires text\" => b, \"-c\" | \"--continue\" => c,";
        let flags: Vec<String> = long_flags(src).into_iter().collect();
        assert_eq!(flags, strings(&["--continue", "--help"]));
    }

    #[test]
    fn typebox_schema_and_interface_members() {
        let schema = "export const SettingsSchema = Type.Object(\n{\n\t$schema: Type.Optional(Type.String()),\n\ttheme: Type.Optional(Type.String({ default: \"a, b\" })),\n},\n{ additionalProperties: true },\n);";
        let (anchor, keys) = ts_members(schema, &["export const SettingsSchema", "export interface Settings"]).unwrap();
        assert_eq!((anchor, keys), ("export const SettingsSchema", strings(&["$schema", "theme"])));
        let interface =
            "export interface Settings extends Omit<A, \"b\"> {\n\tretry?: RetrySettings;\n\tskills?: string[];\n}";
        let (_, keys) = ts_members(interface, &["export interface Settings"]).unwrap();
        assert_eq!(keys, strings(&["retry", "skills"]));
        assert!(ts_members("const x = 1;", &["export interface Settings"]).is_none());
    }

    #[test]
    fn serde_field_names_follow_renames() {
        let src = r#"
/// Doc.
#[derive(Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default, rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Keyed by `provider/modelId`.
    #[serde(default)]
    pub last_changelog_version: Option<BTreeMap<String, Vec<(u8, u8)>>>,
    #[serde(
        default,
        rename = "weird-name"
    )]
    pub renamed: Option<fn(u8) -> u8>,
    #[serde(skip)]
    pub cache: u8,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
    pub r#type: String,
}
pub struct SettingsPatch { pub other: u8 }
"#;
        assert_eq!(
            serde_fields(src, "Settings").unwrap(),
            strings(&["$schema", "lastChangelogVersion", "weird-name", "type"])
        );
        assert_eq!(serde_fields(src, "SettingsPatch").unwrap(), strings(&["other"]));
        assert!(serde_fields(src, "Missing").is_none());
    }

    #[test]
    fn const_arrays_in_both_languages() {
        let ts = "export const BUILTIN_SLASH_COMMANDS: ReadonlyArray<B> = [\n\t{ name: \"settings\", description: \"x\" },\n\t{ name: \"quit\", description: `Quit ${APP}` },\n];";
        assert_eq!(
            const_array_entries(ts, "BUILTIN_SLASH_COMMANDS", Lang::Ts).unwrap(),
            strings(&["settings", "quit"])
        );
        let rust = "pub const BUILTIN_SLASH_COMMANDS: &[(&str, &str, Option<&str>)] = &[\n    (\"model\", \"Select\", Some(\"<m>\")),\n    (\"quit\", \"Quit {app}\", None),\n];";
        assert_eq!(
            const_array_entries(rust, "BUILTIN_SLASH_COMMANDS", Lang::Rust).unwrap(),
            strings(&["model", "quit"])
        );
        let names = "pub const NODE_BUILTINS: &[&str] = &[\n    \"fs\", // comment \"no\"\n    \"fs/promises\",\n];";
        assert_eq!(const_array_literals(names, "NODE_BUILTINS", Lang::Rust).unwrap(), strings(&["fs", "fs/promises"]));
        assert!(const_array_literals(names, "OTHER", Lang::Rust).is_none());
    }

    #[test]
    fn identifiers_skip_comment_lines() {
        let mut words = HashSet::new();
        identifiers("// registerTool\nlet a = pi.getFlag(x); /// getSettings\n  * setLabel", &mut words);
        assert!(words.contains("getFlag") && words.contains("pi"));
        assert!(!words.contains("registerTool") && !words.contains("setLabel"));
    }
}
