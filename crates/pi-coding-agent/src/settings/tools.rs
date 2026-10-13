//! Tool selection lists for `defaultTools`, `--tools`, and `--exclude-tools`
//! (upstream `settings-manager.ts` helpers): plain names, `*` patterns, and `+name`/`-name` deltas.

/// Tools enabled at startup when `defaultTools` does not change them.
pub const DEFAULT_TOOL_NAMES: &[&str] = &["read", "bash", "edit", "write"];

/// Whether an entry is a `+name` or `-name` modifier.
pub fn is_tool_modifier(entry: &str) -> bool {
    entry.starts_with('+') || entry.starts_with('-')
}

/// A tool list is either names and patterns, or only `+name`/`-name` entries with exact names.
pub fn tool_list_error(entries: &[String]) -> Option<String> {
    let modifiers: Vec<&String> = entries.iter().filter(|entry| is_tool_modifier(entry)).collect();
    if modifiers.is_empty() {
        return None;
    }
    if modifiers.len() < entries.len() {
        return Some("tool names cannot be mixed with +name or -name entries".into());
    }
    modifiers
        .iter()
        .find(|entry| entry.contains('*'))
        .map(|entry| format!("+name and -name entries take exact tool names, not patterns: {entry}"))
}

/// Apply the `+name` and `-name` entries of `entries` to `base`, in order.
pub fn apply_tool_modifiers(base: &[String], entries: &[String]) -> Vec<String> {
    let mut tools = base.to_vec();
    for entry in entries.iter().filter(|entry| is_tool_modifier(entry)) {
        let name = &entry[1..];
        let index = tools.iter().position(|tool| tool == name);
        match (entry.starts_with('+'), index) {
            (true, None) if !name.is_empty() => tools.push(name.to_string()),
            (false, Some(index)) => {
                tools.remove(index);
            }
            _ => {}
        }
    }
    tools
}

/// Merge `defaultTools` of two layers: plain names replace, a list of only modifiers appends.
pub fn merge_default_tools(base: Option<&[String]>, overrides: Option<&[String]>) -> Option<Vec<String>> {
    match (base, overrides) {
        (_, None) => base.map(<[String]>::to_vec),
        (Some(base), Some(overrides)) if overrides.iter().all(|entry| is_tool_modifier(entry)) => {
            Some(base.iter().chain(overrides).cloned().collect())
        }
        (_, Some(overrides)) => Some(overrides.to_vec()),
    }
}

/// Resolve a merged `defaultTools` list: plain names replace the defaults, then modifiers apply.
pub fn resolve_default_tools(entries: &[String]) -> Vec<String> {
    let plain: Vec<String> = entries.iter().filter(|entry| !is_tool_modifier(entry)).cloned().collect();
    let base = if !plain.is_empty() || entries.is_empty() {
        plain
    } else {
        DEFAULT_TOOL_NAMES.iter().map(|name| name.to_string()).collect()
    };
    apply_tool_modifiers(&base, entries)
}

/// Whether `name` matches `pattern`, where `*` matches any run of characters.
pub fn tool_pattern_matches(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || name.len() < first.len() + last.len() || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    true
}

/// Whether any pattern in `patterns` matches `name`.
pub fn matches_any_tool(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|pattern| tool_pattern_matches(pattern, name))
}

/// The startup tool selection, as upstream `createAgentSession` computes it from the
/// `defaultTools` setting, `--tools`, `--exclude-tools`, and `--no-tools`.
/// `available` lists the tool names that exist, in their declared order.
pub fn select_tools(
    defaults: &[String],
    tools: Option<&[String]>,
    exclude: &[String],
    no_tools: bool,
    available: &[&str],
) -> Vec<String> {
    let defaults: Vec<String> = if no_tools { Vec::new() } else { defaults.to_vec() };
    let selected: Vec<String> = match tools {
        Some(entries) if entries.iter().any(|entry| is_tool_modifier(entry)) => {
            apply_tool_modifiers(&defaults, entries)
        }
        Some(entries) => {
            let mut names: Vec<String> = Vec::new();
            for entry in entries {
                let matched: Vec<String> = if entry.contains('*') {
                    available
                        .iter()
                        .filter(|name| tool_pattern_matches(entry, name))
                        .map(|name| name.to_string())
                        .collect()
                } else {
                    vec![entry.clone()]
                };
                for name in matched {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
            names
        }
        None => defaults,
    };
    selected.into_iter().filter(|name| !matches_any_tool(exclude, name)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn modifiers_and_merging_follow_upstream() {
        assert_eq!(resolve_default_tools(&list(&["+grep", "-bash"])), ["read", "edit", "write", "grep"]);
        assert_eq!(resolve_default_tools(&list(&["read", "+ls"])), ["read", "ls"]);
        assert!(resolve_default_tools(&[]).is_empty());
        let merged = merge_default_tools(Some(&list(&["read", "bash"])), Some(&list(&["+ls", "-bash"]))).unwrap();
        assert_eq!(resolve_default_tools(&merged), ["read", "ls"]);
        let replaced = merge_default_tools(Some(&list(&["read"])), Some(&list(&["grep"]))).unwrap();
        assert_eq!(replaced, ["grep"]);
        assert!(tool_list_error(&list(&["read", "+bash"])).unwrap().contains("cannot be mixed"));
        assert!(tool_list_error(&list(&["+mcp__*"])).unwrap().contains("not patterns"));
        assert!(tool_list_error(&list(&["read", "mcp__*"])).is_none());
    }

    #[test]
    fn selection_applies_patterns_exclusions_and_no_tools() {
        let defaults = list(DEFAULT_TOOL_NAMES);
        let available = ["read", "bash", "edit", "write", "grep", "find", "ls"];
        assert_eq!(select_tools(&defaults, None, &list(&["b*"]), false, &available), ["read", "edit", "write"]);
        assert_eq!(
            select_tools(&defaults, Some(&list(&["*e*"])), &[], false, &available),
            ["read", "edit", "write", "grep"]
        );
        assert_eq!(select_tools(&defaults, Some(&list(&["+ls"])), &[], true, &available), ["ls"]);
        assert!(select_tools(&defaults, None, &[], true, &available).is_empty());
        assert!(tool_pattern_matches("mcp__*__read", "mcp__srv__read"));
        assert!(!tool_pattern_matches("mcp__*__read", "mcp__srv__write"));
    }
}
