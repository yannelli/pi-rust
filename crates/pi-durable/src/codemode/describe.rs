//! What the model reads about code mode, as in the coding agent's code mode: the tool's fixed description (the script
//! environment and its globals), and the `codemode` prompt section, which lists the tools scripts can call as
//! TypeScript declarations, grouped by namespace and limited to a token budget.

use std::collections::HashSet;
use std::sync::Arc;

use pi_codemode::js::{js_trim, locale_compare, utf16_len};
use pi_codemode::{ToolDeclaration, render_tool_sample, to_codemode_identifier};
use serde_json::json;

use super::models::MODELS_DESCRIPTION;
use crate::harness::ToolRegistration;

/// A group of tools named by a common prefix, such as an MCP server's `mcp__github__`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodemodeNamespace {
    pub name: String,
    pub description: Option<String>,
    pub instructions: Option<String>,
    /// Tool names starting with it belong to the namespace; the longest matching prefix wins.
    pub prefix: String,
}

impl CodemodeNamespace {
    pub fn new(name: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self { name: name.into(), prefix: prefix.into(), ..Self::default() }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }
}

/// Default token budget of the tool declarations in the `codemode` section, as in the coding agent.
pub const DEFAULT_CODEMODE_INLINE_BUDGET: usize = 3000;
/// Characters per token when estimating the cost of a tool section.
const CHARS_PER_TOKEN: usize = 4;

const DESCRIPTION_INTRO: &str = r#"Run JavaScript that calls other tools. The input is raw JavaScript (not JSON, no code fence), run as an async function body in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.
- `await tools.<name>({ ...args })` resolves to a string, or an object if the tool's declaration says so, and rejects with an Error on failure. A tool whose declaration returns an object resolves on failure too when it returned that object; check its fields. Calls still running when the script ends are cancelled.
- The script survives a restart: it resumes where it was, without running finished calls again.
- Optional first line: `// @options: {"max_output_tokens": 10000, "timeout_ms": 60000}`"#;

const GLOBALS: [&str; 4] = [
    "Globals:",
    "- `text(value)`, `image(dataUrlOrImageBlock)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script. With several text items, each starts with a `==> text N/M <==` line, and `console` lines follow the other output in one `<console_output>` block. `image()` also saves the image to a temp file and the result names its path.",
    "- `store(key, value)` and `load(key)` keep JSON values across codemode calls.",
    "- `ALL_TOOLS`, `await searchTools(query, { limit?, namespace? })`, `await describeTool(name)`, `await describeNamespace(name)`: find tools the `codemode` section does not list.",
];

/// The code mode tool's description: the script environment and its globals, the same for every conversation.
pub fn codemode_description(models: bool) -> String {
    let mut parts = vec![DESCRIPTION_INTRO.to_owned(), GLOBALS.join("\n")];
    if models {
        parts.push(MODELS_DESCRIPTION.clone());
    }
    parts.push(
        "The `codemode` section of the system prompt lists the tools scripts can call, by namespace, as far as space allows."
            .to_owned(),
    );
    parts.join("\n\n")
}

/// `toolSample` of a tool: its description and TypeScript declaration. Tools without an output schema resolve to
/// their text output.
pub(super) fn sample_of(tool: &ToolRegistration) -> String {
    let declaration = ToolDeclaration {
        name: tool.name().to_owned(),
        description: Some(tool.tool.description.clone()),
        input_schema: Some(tool.tool.parameters.clone()),
        output_schema: Some(tool.structured_output_schema.clone().unwrap_or_else(|| json!({ "type": "string" }))),
        signature: None,
    };
    render_tool_sample(&declaration, None)
}

/// The namespace of a tool: the one with the longest prefix its name starts with.
pub(super) fn namespace_of<'a>(name: &str, namespaces: &'a [CodemodeNamespace]) -> Option<&'a CodemodeNamespace> {
    let mut found: Option<&CodemodeNamespace> = None;
    for namespace in namespaces {
        let longer = found.is_none_or(|found| utf16_len(&namespace.prefix) > utf16_len(&found.prefix));
        if name.starts_with(&namespace.prefix) && longer {
            found = Some(namespace);
        }
    }
    found
}

/// `### `id` (`raw name`)` followed by the tool's description and declaration.
fn render_tool_section(tool: &ToolRegistration) -> String {
    let id = to_codemode_identifier(tool.name());
    let heading = if id == tool.name() { format!("### `{id}`") } else { format!("### `{id}` (`{}`)", tool.name()) };
    format!("{heading}\n{}", js_trim(&sample_of(tool)))
}

struct Entry {
    name: String,
    section: String,
    cost: usize,
}

struct Group<'a> {
    namespace: Option<&'a CodemodeNamespace>,
    entries: Vec<Entry>,
}

/// Pick the tool sections that fit the budget: in each round every group (tools without a namespace first, then
/// namespaces by name) places its cheapest remaining tool; a group whose next tool does not fit drops out while the
/// others continue. Every namespace is represented before any namespace is complete.
fn select_catalog(groups: &[Group<'_>], budget: usize) -> HashSet<String> {
    let mut queues: Vec<Vec<&Entry>> = groups
        .iter()
        .map(|group| {
            let mut queue: Vec<&Entry> = group.entries.iter().collect();
            queue.sort_by_key(|entry| entry.cost);
            queue.reverse();
            queue
        })
        .filter(|queue| !queue.is_empty())
        .collect();
    let mut shown = HashSet::new();
    let mut remaining = budget;
    while !queues.is_empty() {
        queues.retain_mut(|queue| {
            let Some(next) = queue.last() else { return false };
            if next.cost > remaining {
                return false;
            }
            remaining -= next.cost;
            shown.insert(next.name.clone());
            queue.pop();
            !queue.is_empty()
        });
    }
    shown
}

/// The `codemode` section: the tools scripts can call, grouped by namespace, with their declarations as far as the
/// budget allows. `None` without callable tools.
pub fn codemode_section(
    tools: &[Arc<ToolRegistration>],
    namespaces: &[CodemodeNamespace],
    budget: usize,
) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let mut groups: Vec<(String, Group<'_>)> = vec![(String::new(), Group { namespace: None, entries: Vec::new() })];
    for tool in tools {
        let namespace = namespace_of(tool.name(), namespaces);
        let key = namespace.map(|namespace| format!("ns:{}", namespace.name)).unwrap_or_default();
        let index = match groups.iter().position(|(candidate, _)| *candidate == key) {
            Some(index) => index,
            None => {
                groups.push((key, Group { namespace, entries: Vec::new() }));
                groups.len() - 1
            }
        };
        let section = render_tool_section(tool);
        let cost = utf16_len(&section).div_ceil(CHARS_PER_TOKEN);
        groups[index].1.entries.push(Entry { name: tool.name().to_owned(), section, cost });
    }
    let mut ordered: Vec<Group<'_>> = groups.into_iter().map(|(_, group)| group).collect();
    ordered.sort_by(|a, b| match (a.namespace, b.namespace) {
        (None, _) => std::cmp::Ordering::Less,
        (_, None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => locale_compare(&a.name, &b.name),
    });
    let shown = select_catalog(&ordered, budget);
    let mut sections = vec!["Tools scripts can call (`await tools.<name>(args)`):".to_owned()];
    for Group { namespace, entries } in &ordered {
        if entries.is_empty() {
            continue;
        }
        let visible: Vec<&Entry> = entries.iter().filter(|entry| shown.contains(&entry.name)).collect();
        if let Some(namespace) = namespace {
            let listing = match visible.len() {
                count if count == entries.len() => "",
                0 => " (tools not listed)",
                _ => " (some tools not listed)",
            };
            let description = namespace.description.as_deref().map(js_trim).filter(|text| !text.is_empty());
            let description = description.map(|text| format!("\n{text}")).unwrap_or_default();
            sections.push(format!("## {}{listing}{description}", namespace.name));
        } else if visible.len() < entries.len() {
            sections.push("Some tools are not listed: find them with `searchTools()`.".to_owned());
        }
        sections.extend(visible.iter().map(|entry| entry.section.clone()));
    }
    Some(sections.join("\n\n"))
}
