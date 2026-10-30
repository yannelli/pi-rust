//! The model-facing `codemode` description and how the tool presents the session's loadout.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::declarations::{ToolDeclaration, render_tool_output_type, render_tool_sample};
use crate::host::{CodemodeToolInfo, ToolNamespace};
use crate::identifier::to_codemode_identifier;
use crate::js::{js_stringify, js_trim, locale_compare, quote, utf16_len};

pub const CODEMODE_TOOL_NAME: &str = "codemode";
/// Default token budget for tool declarations in the description (`codemode.inlineBudget`).
pub const DEFAULT_CODEMODE_INLINE_BUDGET: usize = 3000;
/// Characters per token when estimating the cost of a tool section.
const CHARS_PER_TOKEN: usize = 4;

pub const CODEMODE_PROMPT_SNIPPET: &str = "Run JavaScript that calls other tools";
pub const CODEMODE_PROMPT_GUIDELINES: [&str; 1] = [
    "Use codemode to batch independent tool calls (Promise.allSettled), chain them, or filter large output, instead of many separate calls.",
];

const DESCRIPTION_INTRO: &str = "Run JavaScript that calls other tools. The input is raw JavaScript (not JSON, no code fence), run as an async function body in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.
- `await tools.<name>({ ...args })` resolves to a string, or an object if the tool's declaration says so, and rejects with an Error on failure. A tool whose declaration returns an object resolves on failure too when it returned that object; check its fields. Calls still running when the script ends are cancelled.
- Optional first line: `// @options: {\"max_output_tokens\": 10000, \"timeout_ms\": 60000}`";

/// How the tool presents the loadout while active (the `codemode.mode` setting).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeMode {
    /// Declared tools stay declared; their descriptions say how scripts call them.
    #[default]
    On,
    /// Declared tools that scripts can call are hidden from the model and listed in `codemode`.
    Only,
}

/// One line per global. `docs_path` names the reference with the `models` API.
fn describe_globals(models: Option<&str>) -> String {
    let mut lines = vec![
        "Globals:".to_string(),
        "- `text(value)`, `image(dataUrlOrImageBlock)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script. With several text items, each starts with a `==> text N/M <==` line, and `console` lines follow the other output in one `<console_output>` block. `image()` also saves the image to a temp file and the result names its path.".into(),
        "- `store(key, value)` and `load(key)` keep JSON values across codemode calls.".into(),
        "- `ALL_TOOLS`, `await searchTools(query, { limit?, namespace? })`, `await describeTool(name)`, `await describeNamespace(name)`: find unlisted tools, such as MCP tools.".into(),
    ];
    if let Some(docs_path) = models {
        lines.push(format!("- `models`: classifiers and image generation. Read {docs_path} first."));
    }
    lines.join("\n")
}

/// What a script sees of a tool: its description followed by its prompt guidelines. Tools without
/// an output schema resolve to their text output.
pub fn to_codemode_declaration(tool: &CodemodeToolInfo, guidelines: &[String]) -> ToolDeclaration {
    let bullets: Vec<String> = guidelines
        .iter()
        .map(|line| js_trim(line))
        .filter(|line| !line.is_empty())
        .map(|line| format!("- {line}"))
        .collect();
    let description = if bullets.is_empty() {
        tool.description.clone()
    } else {
        format!("{}\n\n{}", js_trim(&tool.description), bullets.join("\n"))
    };
    ToolDeclaration {
        name: tool.name.clone(),
        description: Some(description),
        input_schema: Some(tool.parameters.clone()),
        output_schema: Some(tool.output_schema.clone().unwrap_or_else(|| json!({ "type": "string" }))),
        signature: None,
    }
}

/// Tools a script may call: every given tool except the codemode tool itself.
pub fn codemode_callable_tools(tools: &[CodemodeToolInfo]) -> Vec<CodemodeToolInfo> {
    tools.iter().filter(|tool| tool.name != CODEMODE_TOOL_NAME).cloned().collect()
}

#[derive(Clone, Debug, Default)]
pub struct CodemodeDescriptionOptions {
    /// Declare `models`, pointing to this reference for its API.
    pub models_docs_path: Option<String>,
    /// Tools that are callable but never listed with their declaration (`deferred` exposure).
    pub deferred: HashSet<String>,
    /// Estimated tokens the tool sections may use. Tools that do not fit are left out like deferred
    /// tools. `None` lists every tool that is not deferred.
    pub inline_budget: Option<usize>,
}

/// `### \`id\` (\`raw name\`)` followed by the tool's description and declaration.
fn render_tool_section(declaration: &ToolDeclaration) -> String {
    let id = to_codemode_identifier(&declaration.name);
    let heading =
        if id == declaration.name { format!("### `{id}`") } else { format!("### `{id}` (`{}`)", declaration.name) };
    format!("{heading}\n{}", js_trim(&render_tool_sample(declaration, None)))
}

struct CatalogEntry {
    name: String,
    section: String,
    cost: usize,
}

struct CatalogGroup {
    namespace: Option<ToolNamespace>,
    entries: Vec<CatalogEntry>,
}

/// Pick the sections that fit the budget: each round, every group (tools without a namespace
/// first) places its cheapest remaining tool, and a group whose next tool does not fit drops out.
fn select_catalog(groups: &[CatalogGroup], budget: Option<usize>) -> HashSet<String> {
    let Some(budget) = budget else {
        return groups.iter().flat_map(|group| group.entries.iter().map(|entry| entry.name.clone())).collect();
    };
    let mut queues: Vec<VecDeque<&CatalogEntry>> = groups
        .iter()
        .map(|group| {
            let mut entries: Vec<&CatalogEntry> = group.entries.iter().collect();
            entries.sort_by_key(|entry| entry.cost);
            VecDeque::from(entries)
        })
        .filter(|queue| !queue.is_empty())
        .collect();
    let mut shown = HashSet::new();
    let mut remaining = budget;
    while !queues.is_empty() {
        queues.retain_mut(|queue| {
            let Some(next) = queue.front().copied() else { return false };
            if next.cost > remaining {
                return false;
            }
            remaining -= next.cost;
            shown.insert(next.name.clone());
            queue.pop_front();
            !queue.is_empty()
        });
    }
    shown
}

/// Model-facing description: the globals and one section per listed tool grouped by namespace.
/// Deferred tools are never listed and do not affect the description at all, so it stays the same
/// while tools are registered late (for example by MCP servers that connect).
pub fn create_codemode_description(tools: &[CodemodeToolInfo], options: &CodemodeDescriptionOptions) -> String {
    let listed: Vec<&CodemodeToolInfo> =
        tools.iter().filter(|tool| tool.name != CODEMODE_TOOL_NAME && !options.deferred.contains(&tool.name)).collect();
    let declarations: Vec<ToolDeclaration> =
        listed.iter().map(|tool| to_codemode_declaration(tool, &tool.prompt_guidelines)).collect();
    let mut groups = vec![CatalogGroup { namespace: None, entries: Vec::new() }];
    let mut group_index: HashMap<String, usize> = HashMap::from([(String::new(), 0)]);
    for (tool, declaration) in listed.iter().zip(&declarations) {
        let key = tool.namespace.as_ref().map(|namespace| format!("ns:{}", namespace.name)).unwrap_or_default();
        let index = *group_index.entry(key).or_insert_with(|| {
            groups.push(CatalogGroup { namespace: tool.namespace.clone(), entries: Vec::new() });
            groups.len() - 1
        });
        let section = render_tool_section(declaration);
        let cost = utf16_len(&section).div_ceil(CHARS_PER_TOKEN);
        groups[index].entries.push(CatalogEntry { name: declaration.name.clone(), section, cost });
    }
    groups.sort_by(|a, b| match (&a.namespace, &b.namespace) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, _) => std::cmp::Ordering::Less,
        (_, None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => locale_compare(&a.name, &b.name),
    });
    let shown = select_catalog(&groups, options.inline_budget);

    let mut sections = vec![DESCRIPTION_INTRO.to_string(), describe_globals(options.models_docs_path.as_deref())];
    if declarations.is_empty() {
        return sections.join("\n\n");
    }

    let mut tool_sections = vec!["Nested tools:".to_string()];
    for group in &groups {
        let visible: Vec<&CatalogEntry> = group.entries.iter().filter(|entry| shown.contains(&entry.name)).collect();
        if let Some(namespace) = &group.namespace {
            // Only tools that did not fit the budget count as not listed here.
            let listing = if visible.len() == group.entries.len() {
                ""
            } else if visible.is_empty() {
                " (tools not listed)"
            } else {
                " (some tools not listed)"
            };
            let description = namespace.description.as_deref().map(js_trim).filter(|text| !text.is_empty());
            let suffix = description.map(|text| format!("\n{text}")).unwrap_or_default();
            tool_sections.push(format!("## {}{listing}{suffix}", namespace.name));
        }
        tool_sections.extend(visible.iter().map(|entry| entry.section.clone()));
    }
    sections.push(tool_sections.join("\n\n"));
    sections.join("\n\n")
}

/// The field names of an object schema, `{ output, exit_code, full_output_path? }`, with the fields
/// of object-typed fields `depth` levels deep: `{ content, structuredContent?: { results } }`, and
/// the value of `const` fields: `{ action: "start", id }`. `None` for other schemas.
fn describe_fields(schema: &Value, depth: usize) -> Option<String> {
    let object = schema.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return None;
    }
    let properties = object.get("properties")?.as_object()?;
    let required: Vec<&str> = match object.get("required") {
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let fields: Vec<String> = properties
        .iter()
        .map(|(name, field)| {
            let nested = match field.get("const") {
                Some(value) => Some(js_stringify(value)),
                None if depth > 0 => describe_fields(field, depth - 1),
                None => None,
            };
            let optional = if required.contains(&name.as_str()) { "" } else { "?" };
            format!("{name}{optional}{}", nested.map(|nested| format!(": {nested}")).unwrap_or_default())
        })
        .collect();
    Some(format!("{{ {} }}", fields.join(", ")))
}

/// The field names of each variant of an `anyOf`/`oneOf` whose variants are all objects, joined by
/// `|`.
fn describe_union_fields(schema: &Value) -> Option<String> {
    let object = schema.as_object()?;
    let variants = object.get("anyOf").filter(|value| !value.is_null()).or_else(|| object.get("oneOf"))?;
    let variants = variants.as_array().filter(|variants| !variants.is_empty())?;
    let described: Option<Vec<String>> = variants.iter().map(|variant| describe_fields(variant, 0)).collect();
    described.map(|fields| fields.join(" | "))
}

/// What a script call resolves to, in one line: `a string`, the field names of an object (see
/// [`describe_fields`]), the field names of each object in a union, the rendered type when it fits
/// on one line, or a pointer to `describeTool()` for anything longer, which would otherwise be sent
/// with every request (upstream #11501).
fn describe_output(name: &str, schema: Option<&Value>) -> String {
    let rendered = render_tool_output_type(schema);
    if rendered == "string" {
        return "a string".into();
    }
    let fields = schema.and_then(|schema| describe_fields(schema, 1).or_else(|| describe_union_fields(schema)));
    if let Some(fields) = fields {
        return format!("`{fields}`");
    }
    if !rendered.contains('\n') {
        return format!("`{rendered}`");
    }
    format!("a value; `await describeTool({})` shows its type", quote(name))
}

/// A declared tool's description followed by how scripts call it and what the call resolves to.
pub fn describe_script_call(tool: &CodemodeToolInfo) -> String {
    let declaration = to_codemode_declaration(tool, &[]);
    format!(
        "{}\n\nCodemode: `tools.{}(args)` resolves to {}.",
        js_trim(&tool.description),
        to_codemode_identifier(&tool.name),
        describe_output(&tool.name, declaration.output_schema.as_ref())
    )
}
