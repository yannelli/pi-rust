//! `searchTools()`, `describeTool()`, and `describeNamespace()`: ranked search and lookup over the
//! script's nested tools and their namespaces.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::host::{CodemodeToolInfo, ToolNamespace};
use crate::identifier::to_codemode_identifier;
use crate::sandbox::SandboxFunction;
use crate::tool_search::{DEFAULT_TOOL_SEARCH_LIMIT, search_tools, tool_search_document};

/// Whether `query` names the namespace: its name or one of its aliases, as written or as the same
/// script identifier (`dev-radius` and `dev_radius` are both `dev_radius`).
pub fn is_namespace_name(namespace: &ToolNamespace, query: &str) -> bool {
    let query_id = to_codemode_identifier(query);
    let names = std::iter::once(&namespace.name).chain(&namespace.aliases);
    names.into_iter().any(|name| name == query || to_codemode_identifier(name) == query_id)
}

/// Spread arguments as a list; a missing argument list is empty.
pub(crate) fn spread_args(args: Option<Value>) -> Vec<Value> {
    match args {
        Some(Value::Array(items)) => items,
        Some(other) => vec![other],
        None => Vec::new(),
    }
}

/// `options?.[key] ?? undefined` for a script value.
fn option_field<'a>(options: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    options.and_then(|options| options.get(key)).filter(|value| !value.is_null())
}

fn search(
    tools: &[CodemodeToolInfo],
    samples: &HashMap<String, String>,
    args: Vec<Value>,
) -> Result<Option<Value>, String> {
    let Some(Value::String(query)) = args.first() else {
        return Err("searchTools() expects a query string".into());
    };
    let options = args.get(1);
    let limit = match option_field(options, "limit") {
        None => DEFAULT_TOOL_SEARCH_LIMIT,
        Some(Value::Number(number)) if number.as_f64().is_some_and(|n| n.fract() == 0.0 && n > 0.0) => {
            number.as_f64().map_or(DEFAULT_TOOL_SEARCH_LIMIT, |n| n.min(usize::MAX as f64) as usize)
        }
        Some(_) => return Err("searchTools() limit must be a positive integer".into()),
    };
    let namespace = match option_field(options, "namespace") {
        None => None,
        Some(Value::String(namespace)) => Some(namespace.as_str()).filter(|namespace| !namespace.is_empty()),
        Some(_) => return Err("searchTools() namespace must be a string".into()),
    };
    let documents: Vec<_> = tools
        .iter()
        .filter(|tool| match (namespace, &tool.namespace) {
            (None, _) => true,
            (Some(query), Some(tool_namespace)) => is_namespace_name(tool_namespace, query),
            (Some(_), None) => false,
        })
        .map(tool_search_document)
        .collect();
    let matches: Vec<Value> = search_tools(query, &documents, limit)
        .into_iter()
        .map(|found| {
            let description = samples.get(&found.name).cloned().unwrap_or_default();
            json!({ "name": to_codemode_identifier(&found.name), "description": description })
        })
        .collect();
    Ok(Some(Value::Array(matches)))
}

fn describe_tool(
    tools: &[CodemodeToolInfo],
    samples: &HashMap<String, String>,
    args: Vec<Value>,
) -> Result<Option<Value>, String> {
    let Some(Value::String(name)) = args.first() else {
        return Err("describeTool() expects a tool name".into());
    };
    let tool = tools.iter().find(|tool| tool.name == *name || to_codemode_identifier(&tool.name) == *name);
    Ok(tool.and_then(|tool| samples.get(&tool.name)).map(|sample| Value::String(sample.clone())))
}

fn describe_namespace(tools: &[CodemodeToolInfo], args: Vec<Value>) -> Result<Option<Value>, String> {
    let Some(Value::String(name)) = args.first() else {
        return Err("describeNamespace() expects a namespace name".into());
    };
    let mut found: Option<&ToolNamespace> = None;
    let mut names = Vec::new();
    for tool in tools {
        let Some(namespace) = tool.namespace.as_ref().filter(|namespace| is_namespace_name(namespace, name)) else {
            continue;
        };
        found.get_or_insert(namespace);
        names.push(Value::String(to_codemode_identifier(&tool.name)));
    }
    let Some(namespace) = found else { return Ok(None) };
    let mut result = Map::new();
    result.insert("name".into(), Value::String(namespace.name.clone()));
    if let Some(description) = namespace.description.as_ref().filter(|text| !text.is_empty()) {
        result.insert("description".into(), Value::String(description.clone()));
    }
    if let Some(instructions) = namespace.instructions.as_ref().filter(|text| !text.is_empty()) {
        result.insert("instructions".into(), Value::String(instructions.clone()));
    }
    result.insert("tools".into(), Value::Array(names));
    Ok(Some(Value::Object(result)))
}

/// The discovery globals over `tools`, whose `ALL_TOOLS` descriptions are `samples` by tool name.
pub(crate) fn discovery_globals(
    tools: Arc<Vec<CodemodeToolInfo>>,
    samples: Arc<HashMap<String, String>>,
) -> Vec<SandboxFunction> {
    let (search_tools_list, search_samples) = (tools.clone(), samples.clone());
    let (describe_tools, describe_samples) = (tools.clone(), samples);
    vec![
        SandboxFunction::new("searchTools", move |args, _| {
            let result = search(&search_tools_list, &search_samples, spread_args(args));
            async move { result }
        })
        .with_spread(),
        SandboxFunction::new("describeTool", move |args, _| {
            let result = describe_tool(&describe_tools, &describe_samples, spread_args(args));
            async move { result }
        })
        .with_spread(),
        SandboxFunction::new("describeNamespace", move |args, _| {
            let result = describe_namespace(&tools, spread_args(args));
            async move { result }
        })
        .with_spread(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn namespace(name: &str, aliases: &[&str]) -> ToolNamespace {
        ToolNamespace { aliases: aliases.iter().map(|alias| alias.to_string()).collect(), ..ToolNamespace::new(name) }
    }

    #[test]
    fn matches_namespace_names_identifiers_and_aliases() {
        let radius = namespace("mcp__dev-radius", &["dev-radius"]);
        assert!(is_namespace_name(&radius, "mcp__dev-radius"));
        assert!(is_namespace_name(&radius, "mcp__dev_radius"));
        assert!(is_namespace_name(&radius, "dev-radius"));
        assert!(is_namespace_name(&radius, "dev_radius"));
        assert!(!is_namespace_name(&radius, "radius"));
        // Without an alias the part after `__` is no longer a name.
        assert!(!is_namespace_name(&namespace("mcp__dev-radius", &[]), "dev-radius"));
        assert!(!is_namespace_name(&namespace("docs", &[]), "mcp"));
    }
}
