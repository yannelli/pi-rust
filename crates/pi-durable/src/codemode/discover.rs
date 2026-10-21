//! Globals for finding tools (`searchTools()`, `describeTool()`, `describeNamespace()`), answered from the start
//! data, so a replay sees what the first run saw; and a script's call of a `models` global as a tool call.

use pi_codemode::tool_search::{ToolSearchDocument, search_tools};
use pi_codemode::{CodemodeCallTarget, CodemodeHostCall, to_codemode_identifier};
use serde_json::{Map, Value, json};

use super::docs::{CodemodeStart, DeclaredNamespace, DeclaredTool};
use super::models::model_call;

/// The discovery globals, in the order scripts get them.
pub(super) const DISCOVERY_GLOBALS: [&str; 3] = ["searchTools", "describeTool", "describeNamespace"];
const DEFAULT_SEARCH_LIMIT: usize = 8;

fn matches_namespace(namespace: &DeclaredNamespace, query: &str) -> bool {
    namespace.name == query || to_codemode_identifier(&namespace.name) == to_codemode_identifier(query)
}

/// What a discovery global returns for `call`; `Ok(None)` resolves to `undefined`.
pub(super) fn discover(start: &CodemodeStart, call: &CodemodeHostCall) -> Result<Option<Value>, String> {
    let args = call.args.as_ref().and_then(Value::as_array).cloned().unwrap_or_default();
    let namespaces = &start.namespaces;
    let tools: Vec<&DeclaredTool> = start.tools.iter().filter(|tool| !tool.hidden).collect();
    match call.name.as_str() {
        "describeTool" => {
            let Some(Value::String(name)) = args.first() else {
                return Err("describeTool() expects a tool name".into());
            };
            let tool = tools.iter().find(|tool| tool.name == *name || tool.js_name == *name);
            Ok(tool.map(|tool| Value::String(tool.description.clone())))
        }
        "describeNamespace" => {
            let Some(Value::String(name)) = args.first() else {
                return Err("describeNamespace() expects a namespace name".into());
            };
            let Some(namespace) = namespaces.iter().find(|candidate| matches_namespace(candidate, name)) else {
                return Ok(None);
            };
            let mut described = Map::new();
            described.insert("name".into(), json!(namespace.name));
            if let Some(description) = &namespace.description {
                described.insert("description".into(), json!(description));
            }
            if let Some(instructions) = &namespace.instructions {
                described.insert("instructions".into(), json!(instructions));
            }
            let members: Vec<String> = namespace.tools.iter().map(|tool| to_codemode_identifier(tool)).collect();
            described.insert("tools".into(), json!(members));
            Ok(Some(Value::Object(described)))
        }
        _ => search(&args, namespaces, &tools).map(Some),
    }
}

/// `searchTools(query, { limit?, namespace? })`: BM25 over each tool's names, description, and namespace.
fn search(args: &[Value], namespaces: &[DeclaredNamespace], tools: &[&DeclaredTool]) -> Result<Value, String> {
    let Some(Value::String(query)) = args.first() else { return Err("searchTools() expects a query string".into()) };
    let option = |key: &str| args.get(1).and_then(Value::as_object).and_then(|options| options.get(key));
    let limit = match option("limit") {
        None | Some(Value::Null) => DEFAULT_SEARCH_LIMIT,
        Some(limit) => match limit.as_f64() {
            Some(limit) if limit.fract() == 0.0 && limit > 0.0 => limit as usize,
            _ => return Err("searchTools() limit must be a positive integer".into()),
        },
    };
    let namespace = match option("namespace") {
        None | Some(Value::Null) => None,
        Some(Value::String(namespace)) => Some(namespace.as_str()),
        Some(_) => return Err("searchTools() namespace must be a string".into()),
    };
    let own = |name: &str| namespaces.iter().find(|namespace| namespace.tools.iter().any(|tool| tool == name));
    let documents: Vec<ToolSearchDocument> = tools
        .iter()
        .filter_map(|tool| {
            let own = own(&tool.name);
            if let Some(query) = namespace
                && !own.is_some_and(|own| matches_namespace(own, query))
            {
                return None;
            }
            let text = [
                tool.name.clone(),
                tool.name.replace('_', " "),
                tool.description.clone(),
                own.map(|own| own.name.clone()).unwrap_or_default(),
                own.and_then(|own| own.description.clone()).unwrap_or_default(),
            ];
            Some(ToolSearchDocument { name: tool.name.clone(), text: text.join(" ") })
        })
        .collect();
    let found = search_tools(query, &documents, limit).into_iter().filter_map(|found| {
        let tool = tools.iter().find(|candidate| candidate.name == found.name)?;
        Some(json!({ "name": tool.js_name, "description": tool.description }))
    });
    Ok(Value::Array(found.collect()))
}

/// A script's call of `models.classify()` or `models.generateImages()` as the call of its internal tool.
pub(super) fn as_tool_call(call: CodemodeHostCall) -> CodemodeHostCall {
    let tool = match call.target {
        CodemodeCallTarget::Global => model_call(&call.name),
        CodemodeCallTarget::Tool => None,
    };
    let Some(tool) = tool else { return call };
    let list = call.args.as_ref().and_then(Value::as_array);
    let arg = |index: usize| list.and_then(|list| list.get(index)).cloned().unwrap_or(Value::Null);
    let args = json!({ "model": arg(0), "context": arg(1) });
    CodemodeHostCall { id: call.id, target: CodemodeCallTarget::Tool, name: tool.to_owned(), args: Some(args) }
}
