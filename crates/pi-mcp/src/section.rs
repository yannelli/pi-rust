//! The `mcp_servers` system prompt section: every enabled server with codemode or deferred tools,
//! with how its tools are reached and a one-line summary. Neither codemode nor tool_search lists
//! servers, so the model learns of them here.

use std::collections::BTreeSet;

use crate::config::{McpExposure, McpServerEntry, mcp_namespace};
use crate::resources::locale_cmp;

/// Name of the system prompt section.
pub const MCP_SERVERS_SECTION: &str = "mcp_servers";
/// Characters of a server description in the section, as Codex allows for deferred namespaces.
const MAX_SERVER_DESCRIPTION_CHARS: usize = 250;
/// Characters of the whole section. Descriptions shrink to fit; servers that still do not fit are
/// left out and counted in a closing line.
pub const MAX_SERVERS_SECTION_CHARS: usize = 4096;

/// Exposures the server's tools can have, known from its config before it connects.
pub fn configured_exposures(entry: &McpServerEntry) -> BTreeSet<McpExposure> {
    let mut exposures = BTreeSet::from([entry.config.exposure()]);
    exposures.extend(entry.config.tool_exposures().into_iter().map(|(_, exposure)| exposure));
    exposures
}

/// Whether some tools are declared to the model, so the first prompt waits for the server.
pub fn has_direct_tools(entry: &McpServerEntry) -> bool {
    configured_exposures(entry).contains(&McpExposure::Direct)
}

/// Whether some tools are reached through codemode or tool_search.
pub fn has_indirect_tools(entry: &McpServerEntry) -> bool {
    let exposures = configured_exposures(entry);
    exposures.contains(&McpExposure::Codemode) || exposures.contains(&McpExposure::Deferred)
}

/// What the section needs of a server: its entry and, once connected, its instructions.
#[derive(Clone, Debug)]
pub struct McpServerListing {
    pub entry: McpServerEntry,
    pub instructions: Option<String>,
}

/// Length as JavaScript counts it, in UTF-16 units.
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn truncate(text: &str, max: usize) -> String {
    if js_len(text) <= max {
        return text.into();
    }
    if max <= 1 {
        return String::new();
    }
    let mut kept = String::new();
    for character in text.chars() {
        if js_len(&kept) + character.len_utf16() > max - 1 {
            break;
        }
        kept.push(character);
    }
    format!("{}…", kept.trim_end())
}

/// First line of the configured description, or of the server instructions.
fn server_summary(server: &McpServerListing) -> String {
    let description = server.entry.config.description.as_deref().map(str::trim).filter(|text| !text.is_empty());
    let text = description.or(server.instructions.as_deref()).unwrap_or_default();
    text.split('\n').next().unwrap_or_default().trim().to_string()
}

/// The section, or `None` without such servers. The intro explains only the ways the listed servers use.
pub fn render_servers_section(servers: &[McpServerListing]) -> Option<String> {
    let mut listed: Vec<&McpServerListing> =
        servers.iter().filter(|server| server.entry.config.is_enabled() && has_indirect_tools(&server.entry)).collect();
    if listed.is_empty() {
        return None;
    }
    listed.sort_by(|a, b| locale_cmp(&a.entry.name, &b.entry.name));
    let reaches: Vec<&str> = listed
        .iter()
        .map(|server| {
            if configured_exposures(&server.entry).contains(&McpExposure::Codemode) {
                "codemode"
            } else {
                "tool_search"
            }
        })
        .collect();
    let mut intro = "MCP servers whose tools are not declared to you.".to_string();
    if reaches.contains(&"codemode") {
        intro.push_str(" Call the tools of `codemode` servers from codemode scripts.");
    }
    if reaches.contains(&"tool_search") {
        intro.push_str(" Load the tools of `tool_search` servers with `tool_search`.");
    }
    let heads: Vec<String> = listed
        .iter()
        .zip(&reaches)
        .map(|(server, reach)| format!("- {} ({reach})", mcp_namespace(&server.entry.name)))
        .collect();
    let omitted = |count: usize| -> Vec<String> {
        if count == 0 {
            return Vec::new();
        }
        let plural = if count == 1 { "" } else { "s" };
        vec![format!("- … {count} more server{plural}; find their tools with searchTools()")]
    };
    // Characters of the intro, the first `kept` server lines without descriptions, and the omission line.
    let size = |kept: usize| {
        let mut lines = vec![intro.clone()];
        lines.extend(heads[..kept].iter().cloned());
        lines.extend(omitted(listed.len() - kept));
        js_len(&lines.join("\n"))
    };
    let mut kept = listed.len();
    while kept > 0 && size(kept) > MAX_SERVERS_SECTION_CHARS {
        kept -= 1;
    }
    // Each description also takes a ": " separator.
    let room = MAX_SERVERS_SECTION_CHARS.saturating_sub(size(kept)).checked_div(kept);
    let per_server = room.map_or(0, |room| room.saturating_sub(2).min(MAX_SERVER_DESCRIPTION_CHARS));
    let mut lines = vec![intro.clone()];
    for (server, head) in listed.iter().zip(&heads).take(kept) {
        let summary = if per_server > 0 { truncate(&server_summary(server), per_server) } else { String::new() };
        lines.push(if summary.is_empty() { head.clone() } else { format!("{head}: {summary}") });
    }
    lines.extend(omitted(listed.len() - kept));
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerConfig;

    fn server(name: &str, description: Option<&str>, exposure: Option<McpExposure>) -> McpServerListing {
        let mut config = McpServerConfig::stdio("x", Vec::new());
        config.description = description.map(str::to_string);
        config.exposure = exposure;
        McpServerListing { entry: McpServerEntry::new(name, config, "test"), instructions: None }
    }

    #[test]
    fn lists_servers_with_how_their_tools_are_reached_and_the_first_line_of_their_description() {
        let plain = McpServerListing { instructions: Some("From instructions.".into()), ..server("plain", None, None) };
        let section = render_servers_section(&[
            server("docs", Some("Docs search.\nMore."), None),
            server("later", None, Some(McpExposure::Deferred)),
            server("direct", Some("Declared."), Some(McpExposure::Direct)),
            plain,
        ])
        .unwrap();
        let lines: Vec<&str> = section.split('\n').skip(1).collect();
        assert_eq!(
            lines,
            [
                "- mcp__docs (codemode): Docs search.",
                "- mcp__later (tool_search)",
                "- mcp__plain (codemode): From instructions."
            ]
        );
        assert_eq!(render_servers_section(&[server("direct", Some("Declared."), Some(McpExposure::Direct))]), None);
    }

    #[test]
    fn shortens_descriptions_to_fit_the_size_limit() {
        let description = "x".repeat(400);
        let servers: Vec<_> =
            (0..40).map(|index| server(&format!("server{index}"), Some(&description), None)).collect();
        let section = render_servers_section(&servers).unwrap();
        assert!(js_len(&section) <= MAX_SERVERS_SECTION_CHARS);
        assert_eq!(section.split('\n').count(), 41);
        assert!(section.contains("- mcp__server39 (codemode): x"));
    }

    #[test]
    fn leaves_out_the_last_servers_when_their_names_alone_do_not_fit() {
        let servers: Vec<_> =
            (0..200).map(|index| server(&format!("server-with-a-long-name-{index}"), Some("desc"), None)).collect();
        let section = render_servers_section(&servers).unwrap();
        assert!(js_len(&section) <= MAX_SERVERS_SECTION_CHARS);
        let lines: Vec<&str> = section.split('\n').collect();
        let last = lines.last().unwrap();
        let count = last
            .strip_prefix("- … ")
            .and_then(|rest| rest.strip_suffix(" more servers; find their tools with searchTools()"));
        let omitted: usize = count.unwrap().parse().unwrap();
        assert_eq!(lines.len() - 2 + omitted, 200);
    }
}
