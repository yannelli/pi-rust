//! Tool discovery: a BM25 ranker over tool metadata, shared by `searchTools()` in codemode scripts
//! and the `tool_search` tool, which loads the matches of tools that are not declared to the model.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::host::{CodemodeToolInfo, ToolExposure, ToolNamespace};
use crate::js::js_trim;

pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
pub const DEFAULT_TOOL_SEARCH_LIMIT: usize = 8;

/// The `tool_search` description. It does not list the searchable tools, so it stays the same while
/// tools are registered, for example when MCP servers connect.
pub const TOOL_SEARCH_DESCRIPTION: &str = "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nSome of the tools, such as tools of MCP servers, may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search`.";
pub const TOOL_SEARCH_PROMPT_SNIPPET: &str = "Search for tools that are not loaded yet and load the matches";

/// JSON Schema of the `tool_search` arguments.
pub fn tool_search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Search query for deferred tools." },
            "limit": {
                "type": "number",
                "description": format!("Maximum number of tools to return. Defaults to {DEFAULT_TOOL_SEARCH_LIMIT}."),
            },
        },
        "required": ["query"],
    })
}

/// A tool as the ranker sees it: its name and the text built by [`create_tool_search_document`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSearchDocument {
    pub name: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolSearchMatch {
    pub name: String,
    pub score: f64,
}

/// Ranks tools for a query. BM25 today; a hybrid ranker with embeddings can replace it.
pub trait ToolRanker {
    fn rank(&self, query: &str, documents: &[ToolSearchDocument], limit: usize) -> Vec<ToolSearchMatch>;
}

const STOP_WORDS: [&str; 21] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on", "or", "that", "the",
    "this", "to", "with",
];

/// Naive singular form, so `issues` matches `issue` and `searches` matches `search`.
fn stem(term: &str) -> String {
    let len = term.len();
    if len > 4 && term.ends_with("ies") {
        return format!("{}y", &term[..len - 3]);
    }
    if len > 4 && ["ches", "shes", "sses", "xes", "zes"].iter().any(|suffix| term.ends_with(suffix)) {
        return term[..len - 2].into();
    }
    if len > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term[..len - 1].into();
    }
    term.into()
}

/// Insert a space wherever `split(prev, current, next)` holds, like a global regex replace.
fn split_case(text: &str, split: impl Fn(char, char, Option<char>) -> bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    for (index, &c) in chars.iter().enumerate() {
        if index > 0 && split(chars[index - 1], c, chars.get(index + 1).copied()) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Lowercase terms, split at camelCase boundaries and non-alphanumerics, without stop words.
pub fn tokenize(text: &str) -> Vec<String> {
    let lower_upper =
        split_case(text, |prev, c, _| (prev.is_ascii_lowercase() || prev.is_ascii_digit()) && c.is_ascii_uppercase());
    let acronym = split_case(&lower_upper, |prev, c, next| {
        prev.is_ascii_uppercase() && c.is_ascii_uppercase() && next.is_some_and(|next| next.is_ascii_lowercase())
    });
    acronym
        .to_lowercase()
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|term| !term.is_empty() && !STOP_WORDS.contains(term))
        .map(stem)
        .collect()
}

/// Schema descriptions and property names, recursively.
fn schema_text(schema: &Value, parts: &mut Vec<String>) {
    let Some(schema) = schema.as_object() else { return };
    if let Some(Value::String(description)) = schema.get("description") {
        parts.push(description.clone());
    }
    if let Some(Value::Object(properties)) = schema.get("properties") {
        for (name, property) in properties {
            parts.push(name.clone());
            schema_text(property, parts);
        }
    }
    if let Some(items) = schema.get("items") {
        schema_text(items, parts);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(variants)) = schema.get(key) {
            for variant in variants {
                schema_text(variant, parts);
            }
        }
    }
}

/// Search text of a tool: the name, the name with `_` as spaces, the description, schema
/// descriptions and property names, and the namespace with its description and instructions.
pub fn create_tool_search_document(
    name: &str,
    description: &str,
    parameters: &Value,
    namespace: Option<&ToolNamespace>,
) -> ToolSearchDocument {
    let mut parts = vec![name.to_string(), name.replace('_', " "), description.to_string()];
    schema_text(parameters, &mut parts);
    if let Some(namespace) = namespace {
        parts.push(namespace.name.clone());
        parts.push(namespace.description.clone().unwrap_or_default());
        parts.push(namespace.instructions.clone().unwrap_or_default());
    }
    let text: Vec<String> = parts.into_iter().filter(|part| !js_trim(part).is_empty()).collect();
    ToolSearchDocument { name: name.into(), text: text.join(" ") }
}

/// The search document of a host tool, with its namespace.
pub fn tool_search_document(tool: &CodemodeToolInfo) -> ToolSearchDocument {
    create_tool_search_document(&tool.name, &tool.description, &tool.parameters, tool.namespace.as_ref())
}

/// Okapi BM25. Ties keep document order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bm25Ranker {
    pub k1: f64,
    pub b: f64,
}

impl Default for Bm25Ranker {
    fn default() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }
}

impl ToolRanker for Bm25Ranker {
    fn rank(&self, query: &str, documents: &[ToolSearchDocument], limit: usize) -> Vec<ToolSearchMatch> {
        let mut seen = HashSet::new();
        let query_terms: Vec<String> = tokenize(query).into_iter().filter(|term| seen.insert(term.clone())).collect();
        if query_terms.is_empty() || documents.is_empty() || limit == 0 {
            return Vec::new();
        }
        let term_counts: Vec<HashMap<String, usize>> = documents
            .iter()
            .map(|document| {
                let mut counts = HashMap::new();
                for term in tokenize(&document.text) {
                    *counts.entry(term).or_insert(0) += 1;
                }
                counts
            })
            .collect();
        let lengths: Vec<f64> = term_counts.iter().map(|counts| counts.values().sum::<usize>() as f64).collect();
        let total = documents.len() as f64;
        let average = lengths.iter().sum::<f64>() / total;
        let average = if average == 0.0 || average.is_nan() { 1.0 } else { average };
        let idf: HashMap<&str, f64> = query_terms
            .iter()
            .map(|term| {
                let frequency = term_counts.iter().filter(|counts| counts.contains_key(term)).count() as f64;
                (term.as_str(), (1.0 + (total - frequency + 0.5) / (frequency + 0.5)).ln())
            })
            .collect();
        let mut matches: Vec<ToolSearchMatch> = Vec::new();
        for (index, document) in documents.iter().enumerate() {
            let mut score = 0.0;
            for term in &query_terms {
                let Some(&count) = term_counts[index].get(term) else { continue };
                let count = count as f64;
                let norm = self.k1 * (1.0 - self.b + self.b * lengths[index] / average);
                score += idf[term.as_str()] * (count * (self.k1 + 1.0)) / (count + norm);
            }
            if score > 0.0 {
                matches.push(ToolSearchMatch { name: document.name.clone(), score });
            }
        }
        matches.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        matches.truncate(limit);
        matches
    }
}

/// Rank `documents` for `query` with BM25.
pub fn search_tools(query: &str, documents: &[ToolSearchDocument], limit: usize) -> Vec<ToolSearchMatch> {
    Bm25Ranker::default().rank(query, documents, limit)
}

/// Whether `tool_search` can load a tool with this exposure.
pub fn is_searchable(exposure: ToolExposure) -> bool {
    matches!(exposure, ToolExposure::Codemode | ToolExposure::Deferred)
}

/// A match of `tool_search`: the tool name and its description.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSearchResultTool {
    pub name: String,
    pub description: String,
}

/// Check `tool_search` arguments and return the limit to use.
pub fn tool_search_limit(query: &str, limit: Option<f64>) -> Result<usize, String> {
    if js_trim(query).is_empty() {
        return Err("query must not be empty".into());
    }
    match limit {
        None => Ok(DEFAULT_TOOL_SEARCH_LIMIT),
        Some(limit) if limit.fract() == 0.0 && limit > 0.0 => Ok(limit.min(usize::MAX as f64) as usize),
        Some(_) => Err("limit must be a positive integer".into()),
    }
}

/// Rank the searchable tools that are not active yet. The caller activates the matches, so the
/// next model call declares them and the change is recorded on the branch.
pub fn search_deferred_tools(
    tools: &[(CodemodeToolInfo, ToolExposure)],
    active: &[String],
    query: &str,
    limit: usize,
) -> Vec<ToolSearchResultTool> {
    let candidates: Vec<&CodemodeToolInfo> = tools
        .iter()
        .filter(|(tool, exposure)| is_searchable(*exposure) && !active.contains(&tool.name))
        .map(|(tool, _)| tool)
        .collect();
    let documents: Vec<ToolSearchDocument> = candidates.iter().map(|tool| tool_search_document(tool)).collect();
    search_tools(query, &documents, limit)
        .into_iter()
        .map(|found| ToolSearchResultTool {
            description: candidates
                .iter()
                .find(|tool| tool.name == found.name)
                .map(|tool| tool.description.clone())
                .unwrap_or_default(),
            name: found.name,
        })
        .collect()
}

/// The text `tool_search` returns for its matches.
pub fn format_tool_search_result(tools: &[ToolSearchResultTool]) -> String {
    if tools.is_empty() {
        return "No matching tools found.".into();
    }
    let lines: Vec<String> = tools
        .iter()
        .map(|tool| {
            let first = js_trim(&tool.description).split('\n').next().unwrap_or("");
            format!("- {}: {}", tool.name, first.strip_suffix('\r').unwrap_or(first))
        })
        .collect();
    let plural = if tools.len() == 1 { "" } else { "s" };
    format!("Loaded {} tool{plural}. They are available from your next call:\n{}", tools.len(), lines.join("\n"))
}
