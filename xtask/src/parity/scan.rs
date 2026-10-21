//! Lexical helpers shared by the parity parsers: skipping strings and comments, balanced
//! groups, and the member names of a TypeScript object or interface body.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Ts,
    Rust,
}

/// The index just past the string, character literal, or comment that starts at `i`, if any.
pub fn skip_lexeme(src: &str, i: usize, lang: Lang) -> Option<usize> {
    let b = src.as_bytes();
    let next = b.get(i + 1).copied();
    match b[i] {
        b'/' if next == Some(b'/') => Some(src[i..].find('\n').map_or(src.len(), |n| i + n)),
        b'/' if next == Some(b'*') => Some(src[i + 2..].find("*/").map_or(src.len(), |n| i + n + 4)),
        b'"' => Some(skip_quoted(b, i, b'"')),
        b'\'' if lang == Lang::Ts => Some(skip_quoted(b, i, b'\'')),
        b'`' if lang == Lang::Ts => Some(skip_template(src, i)),
        b'\'' if lang == Lang::Rust => rust_char_literal(src, i),
        b'r' if lang == Lang::Rust && i.checked_sub(1).is_none_or(|p| !is_ident_byte(b[p])) => skip_raw_string(src, i),
        _ => None,
    }
}

pub fn starts_with_any(text: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| text.starts_with(prefix))
}

pub fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

fn skip_quoted(b: &[u8], start: usize, quote: u8) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            byte if byte == quote => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

fn skip_template(src: &str, start: usize) -> usize {
    let b = src.as_bytes();
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'`' => return i + 1,
            b'$' if b.get(i + 1) == Some(&b'{') => {
                i = matching_close(src, i + 1, Lang::Ts).map_or(b.len(), |end| end + 1)
            }
            _ => i += 1,
        }
    }
    b.len()
}

/// A Rust character literal such as `'a'` or `'\n'`; lifetimes are not literals.
fn rust_char_literal(src: &str, i: usize) -> Option<usize> {
    let rest = &src[i + 1..];
    let first = rest.chars().next()?;
    if first == '\\' {
        let after = i + 2 + rest[1..].chars().next()?.len_utf8();
        return Some(src[after..].find('\'').map_or(src.len(), |n| after + n + 1));
    }
    let end = i + 1 + first.len_utf8();
    (src.as_bytes().get(end) == Some(&b'\'')).then_some(end + 1)
}

/// A raw string `r"..."` or `r#"..."#` starting at the `r`.
fn skip_raw_string(src: &str, i: usize) -> Option<usize> {
    let rest = &src[i + 1..];
    let hashes = rest.bytes().take_while(|&byte| byte == b'#').count();
    if rest.as_bytes().get(hashes) != Some(&b'"') {
        return None;
    }
    let closing = format!("\"{}", "#".repeat(hashes));
    let body = i + 1 + hashes + 1;
    Some(src[body..].find(&closing).map_or(src.len(), |n| body + n + closing.len()))
}

/// The index of the bracket that closes the one at `open`.
pub fn matching_close(src: &str, open: usize, lang: Lang) -> Option<usize> {
    let b = src.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        if let Some(end) = skip_lexeme(src, i, lang) {
            i = end;
            continue;
        }
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The text between the first bracket after `anchor` and its closing bracket.
pub fn group_after(src: &str, anchor: usize, open: u8, lang: Lang) -> Option<&str> {
    let start = anchor + src[anchor..].bytes().position(|byte| byte == open)?;
    let end = matching_close(src, start, lang)?;
    Some(&src[start + 1..end])
}

/// The content of a terminated string literal that starts at `i` (without its quotes).
pub fn literal_at(src: &str, i: usize, lang: Lang) -> Option<&str> {
    let quote = src.as_bytes()[i];
    if !matches!(quote, b'"' | b'\'' | b'`') {
        return None;
    }
    let end = skip_lexeme(src, i, lang)?;
    (end > i + 1 && src.as_bytes()[end - 1] == quote).then(|| &src[i + 1..end - 1])
}

/// String literals directly inside `content` (not nested in brackets).
pub fn top_level_literals(content: &str, lang: Lang) -> Vec<String> {
    let b = content.as_bytes();
    let mut found = Vec::new();
    let (mut depth, mut i) = (0usize, 0);
    while i < b.len() {
        if let Some(end) = skip_lexeme(content, i, lang) {
            if depth == 0
                && let Some(text) = literal_at(content, i, lang)
            {
                found.push(text.to_string());
            }
            i = end;
            continue;
        }
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    found
}

/// The first string literal inside each `{...}` or `(...)` group directly inside `content`.
pub fn first_literal_per_group(content: &str, lang: Lang) -> Vec<String> {
    let b = content.as_bytes();
    let mut names = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if let Some(end) = skip_lexeme(content, i, lang) {
            i = end;
        } else if matches!(b[i], b'{' | b'(') {
            let end = matching_close(content, i, lang).unwrap_or(b.len());
            if let Some(name) = first_literal(&content[i + 1..end.min(b.len())], lang) {
                names.push(name);
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    names
}

fn first_literal(text: &str, lang: Lang) -> Option<String> {
    let mut i = 0;
    while i < text.len() {
        if let Some(literal) = literal_at(text, i, lang) {
            return Some(literal.to_string());
        }
        i = skip_lexeme(text, i, lang).unwrap_or(i + 1);
    }
    None
}

const MODIFIERS: &[&str] = &["readonly", "static", "public", "private", "protected", "async", "get", "set", "abstract"];

/// Member names directly inside a TypeScript object literal, object type, or interface body.
pub fn member_names(body: &str) -> Vec<String> {
    let b = body.as_bytes();
    let (mut names, mut depth, mut expecting, mut i) = (Vec::new(), 0usize, true, 0);
    while i < b.len() {
        if let Some(end) = skip_lexeme(body, i, Lang::Ts) {
            if expecting && depth == 0 {
                if let Some(name) = literal_at(body, i, Lang::Ts) {
                    if ends_member_name(body, end) {
                        names.push(name.to_string());
                    }
                    expecting = false;
                } else if b[i] != b'/' {
                    expecting = false;
                }
            }
            i = end;
            continue;
        }
        match b[i] {
            b'(' | b'[' | b'{' => {
                depth += 1;
                expecting = false;
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' | b';' if depth == 0 => expecting = true,
            byte if expecting && depth == 0 && is_ident_byte(byte) => {
                let end = i + b[i..].iter().take_while(|&&byte| is_ident_byte(byte)).count();
                let name = &body[i..end];
                if ends_member_name(body, end) {
                    names.push(name.to_string());
                    expecting = false;
                } else if !MODIFIERS.contains(&name) {
                    expecting = false;
                }
                i = end;
                continue;
            }
            byte if expecting && depth == 0 && !byte.is_ascii_whitespace() => expecting = false,
            _ => {}
        }
        i += 1;
    }
    names
}

/// Whether the text after a member name starts a property or method (`:`, `?`, `(`, `<`).
fn ends_member_name(body: &str, end: usize) -> bool {
    body[end..].trim_start_matches([' ', '\t']).starts_with([':', '?', '(', '<', '!'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_strings_comments_and_templates() {
        let src = "(a \"x)\" 'y)' `z ${ f(\")\") } )` /* ) */ // )\n b)";
        assert_eq!(matching_close(src, 0, Lang::Ts), Some(src.len() - 1));
    }

    #[test]
    fn rust_lifetimes_and_chars_are_not_strings() {
        let src = "( &'static str, '(' , r#\"a)\"# )";
        assert_eq!(matching_close(src, 0, Lang::Rust), Some(src.len() - 1));
    }

    #[test]
    fn member_names_of_an_interface() {
        let body = r#"
	/** doc with "quotes", commas; and (parens */
	on(event: "a", handler: Handler<A, B>): () => void;
	on(event: "b"): void;
	registerTool<TParams extends TSchema = TSchema, TDetails = unknown>(
		tool: ToolDefinition<TParams, TDetails>,
	): void;
	readonly flag?: boolean;
	"quoted-key": string;
	nested: { inner: string; other: number };
	events: EventBus;
	[key: string]: unknown;
"#;
        assert_eq!(
            member_names(body),
            ["on", "on", "registerTool", "flag", "quoted-key", "nested", "events"].map(String::from)
        );
    }

    #[test]
    fn member_names_of_a_schema_object() {
        let body = r#"
		$schema: Type.Optional(Type.String({ description: "a, b: c" })),
		theme: Type.Optional(Type.Union([Type.Literal("x"), Type.Literal("y")], { default: 1 })),
		...spread,
		"quoted": 1,
		end: 2
"#;
        assert_eq!(member_names(body), ["$schema", "theme", "quoted", "end"].map(String::from));
    }

    #[test]
    fn literals_and_groups() {
        let rust = r#"&[("a", "x"), ("b", "y"), /* ("c", "z") */ ("d", "w")]"#;
        let content = group_after(rust, 0, b'[', Lang::Rust).unwrap();
        assert_eq!(first_literal_per_group(content, Lang::Rust), ["a", "b", "d"].map(String::from));
        let ts = "[{ name: \"s\", description: `Quit ${APP}` }, { name: \"t\" }]";
        let content = group_after(ts, 0, b'[', Lang::Ts).unwrap();
        assert_eq!(first_literal_per_group(content, Lang::Ts), ["s", "t"].map(String::from));
        assert_eq!(top_level_literals("\"a\", (\"no\"), \"b\"", Lang::Rust), ["a", "b"].map(String::from));
    }
}
