//! Names of MCP tools as the model sees them.

use crate::oauth::pkce::sha256;

/// Provider tool names are limited to 64 characters of `[A-Za-z0-9_-]`.
pub const MAX_TOOL_NAME_LENGTH: usize = 64;

/// MCP resource tools, which reach every server with resources.
pub const LIST_MCP_RESOURCES_TOOL: &str = "list_mcp_resources";
pub const LIST_MCP_RESOURCE_TEMPLATES_TOOL: &str = "list_mcp_resource_templates";
pub const READ_MCP_RESOURCE_TOOL: &str = "read_mcp_resource";

/// Whether a tool comes from MCP: a server tool (`mcp__<server>__<tool>`) or a resource tool.
pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with("mcp__")
        || [LIST_MCP_RESOURCES_TOOL, LIST_MCP_RESOURCE_TEMPLATES_TOOL, READ_MCP_RESOURCE_TOOL].contains(&name)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `mcp__<server>__<tool>` with everything but `[A-Za-z0-9_]` replaced by `_`, so the name is also
/// the identifier codemode scripts call it by. Names over 64 characters, or names `is_taken` reports
/// as used by another tool, are shortened and get a hash suffix.
pub fn create_mcp_tool_name(server: &str, tool: &str, is_taken: impl Fn(&str) -> bool) -> String {
    let name = format!("mcp__{}__{}", identifier(server), identifier(tool));
    if name.len() <= MAX_TOOL_NAME_LENGTH && !is_taken(&name) {
        return name;
    }
    let hash = hex(&sha256(format!("{server}\0{tool}").as_bytes()))[..8].to_string();
    let keep = MAX_TOOL_NAME_LENGTH - hash.len() - 1;
    format!("{}_{hash}", &name[..name.len().min(keep)])
}

/// `text` with everything but `[A-Za-z0-9_]` replaced by `_`.
fn identifier(text: &str) -> String {
    let mut name = String::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            name.push(character);
        } else {
            // One `_` per UTF-16 unit, like the JavaScript regular expression.
            name.extend(std::iter::repeat_n('_', character.len_utf16()));
        }
    }
    name
}

/// The server part of its tool names. Servers with the same key would get the same tool names.
pub fn server_key(server: &str) -> String {
    identifier(server)
}

/// Split `mcp__<server>__<tool>` into the server and tool parts shown in labels.
pub fn split_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let index = rest.find("__")?;
    let (server, tool) = (&rest[..index], &rest[index + 2..]);
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_provider_safe_tool_names() {
        let free = |_: &str| false;
        assert_eq!(create_mcp_tool_name("docs", "search", free), "mcp__docs__search");
        assert_eq!(create_mcp_tool_name("my-server", "get.item/v2", free), "mcp__my_server__get_item_v2");
        let long = create_mcp_tool_name("server", &"x".repeat(100), free);
        assert_eq!(long.len(), 64);
        let (head, hash) = long.rsplit_once('_').unwrap();
        assert!(
            head.starts_with("mcp__server__x") && head.trim_start_matches("mcp__server__").chars().all(|c| c == 'x')
        );
        assert!(hash.len() == 8 && hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(create_mcp_tool_name("server", &format!("{}y", "x".repeat(100)), free), long);
        // Names that sanitize to one already taken by another tool get a hash suffix.
        let taken = create_mcp_tool_name("s", "a_b", free);
        let second = create_mcp_tool_name("s", "a-b", |name| name == taken);
        assert!(second.starts_with("mcp__s__a_b_") && second.len() == "mcp__s__a_b_".len() + 8, "{second}");
        assert_eq!(split_mcp_tool_name("mcp__docs__get__item"), Some(("docs", "get__item")));
        assert!(is_mcp_tool_name("read_mcp_resource") && !is_mcp_tool_name("read"));
    }

    #[test]
    fn server_keys_match_the_server_part_of_tool_names() {
        for server in ["a-b", "a.b", "a b", "a_b"] {
            assert_eq!(server_key(server), "a_b");
            assert_eq!(create_mcp_tool_name(server, "t", |_| false), "mcp__a_b__t");
        }
        assert_eq!(server_key("é"), "_");
        assert_eq!(server_key("😀"), "__");
    }
}
