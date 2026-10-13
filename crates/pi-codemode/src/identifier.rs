//! Script identifiers for tool names.

/// The identifier a script uses for a tool: characters that are not valid in a JavaScript
/// identifier become `_`. `mcp__docs__search` stays as is, `my-tool` becomes `my_tool`.
pub fn to_codemode_identifier(name: &str) -> String {
    let mut identifier = String::with_capacity(name.len());
    for c in name.chars() {
        let valid = c.is_ascii_alphabetic() || c == '_' || c == '$' || (!identifier.is_empty() && c.is_ascii_digit());
        identifier.push(if valid { c } else { '_' });
    }
    if identifier.is_empty() { "_".into() } else { identifier }
}

/// Whether `name` is a plain JavaScript identifier (`^[A-Za-z_$][A-Za-z0-9_$]*$`).
pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' || first == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_invalid_characters() {
        assert_eq!(to_codemode_identifier("mcp__docs__search"), "mcp__docs__search");
        assert_eq!(to_codemode_identifier("my-tool"), "my_tool");
        assert_eq!(to_codemode_identifier("mcp__dev-radius__search"), "mcp__dev_radius__search");
        assert_eq!(to_codemode_identifier("1abc"), "_abc");
        assert_eq!(to_codemode_identifier("héllo"), "h_llo");
        assert_eq!(to_codemode_identifier(""), "_");
        assert!(is_identifier("$a1"));
        assert!(!is_identifier("1a"));
        assert!(!is_identifier(""));
    }
}
