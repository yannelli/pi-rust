//! TypeScript declarations of the script-visible API.

use serde_json::Value;

use crate::identifier::to_codemode_identifier;
use crate::js::js_trim;
use crate::schema::{INDENT, schema_to_type};

/// Largest rendered input type, in characters, before it becomes `unknown`.
pub const DEFAULT_INPUT_SCHEMA_MAX_CHARS: usize = 16_000;

/// What a declaration needs of a tool or global. `signature` (globals only) is a TypeScript
/// parameter list and return type that replaces the rendering from the schemas.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolDeclaration {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    pub signature: Option<String>,
}

impl ToolDeclaration {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), ..Self::default() }
    }
}

/// Tools become members of `declare const tools`, globals `declare function` statements, and
/// `ns.member` globals members of `declare const ns`.
pub fn render_declarations(tools: &[ToolDeclaration], globals: &[ToolDeclaration]) -> String {
    let mut sections = Vec::new();
    if !tools.is_empty() {
        let members: Vec<String> = tools
            .iter()
            .map(|tool| {
                format!(
                    "{}{INDENT}{}",
                    doc_comment(tool.description.as_deref(), INDENT),
                    render_tool_signature(tool, None)
                )
            })
            .collect();
        sections.push(format!("declare const tools: {{\n{}\n}};", members.join("\n")));
    }
    let mut namespaces: Vec<(String, Vec<String>)> = Vec::new();
    for global in globals {
        let Some((namespace, member)) = global.name.split_once('.') else {
            sections.push(render_global(&format!("declare function {}", global.name), global, ""));
            continue;
        };
        let rendered = render_global(member, global, INDENT);
        match namespaces.iter_mut().find(|(name, _)| name == namespace) {
            Some((_, members)) => members.push(rendered),
            None => namespaces.push((namespace.into(), vec![rendered])),
        }
    }
    for (namespace, members) in namespaces {
        sections.push(format!("declare const {namespace}: {{\n{}\n}};", members.join("\n")));
    }
    sections.join("\n\n")
}

/// One tool as a member of the `tools` object: `name(args: T): Promise<R>;`. Input types longer
/// than `input_max_chars` render as `unknown`.
pub fn render_tool_signature(tool: &ToolDeclaration, input_max_chars: Option<usize>) -> String {
    let input = match &tool.input_schema {
        None => "unknown".to_string(),
        Some(schema) => schema_to_type(schema, Some(input_max_chars.unwrap_or(DEFAULT_INPUT_SCHEMA_MAX_CHARS))),
    };
    format!(
        "{}(args: {input}): Promise<{}>;",
        to_codemode_identifier(&tool.name),
        render_tool_output_type(tool.output_schema.as_ref())
    )
}

/// A tool's description followed by its declaration, for tool listings and `ALL_TOOLS` entries.
pub fn render_tool_sample(tool: &ToolDeclaration, input_max_chars: Option<usize>) -> String {
    let description = tool.description.as_deref().map(js_trim).unwrap_or("");
    let declaration = format!("declare const tools: {{ {} }};", render_tool_signature(tool, input_max_chars));
    format!("{description}\n\ncodemode tool declaration:\n```ts\n{declaration}\n```")
}

/// The type a tool call resolves to: the output schema's type, and `unknown` without a schema.
pub fn render_tool_output_type(schema: Option<&Value>) -> String {
    schema.map_or_else(|| "unknown".into(), |schema| schema_to_type(schema, None))
}

fn render_global(head: &str, global: &ToolDeclaration, indent: &str) -> String {
    let doc = doc_comment(global.description.as_deref(), indent);
    if let Some(signature) = &global.signature {
        return format!("{doc}{indent}{head}{signature};");
    }
    let render = |schema: &Option<Value>| schema.as_ref().map_or_else(|| "unknown".into(), |s| schema_to_type(s, None));
    format!("{doc}{indent}{head}(args: {}): Promise<{}>;", render(&global.input_schema), render(&global.output_schema))
}

fn doc_comment(description: Option<&str>, indent: &str) -> String {
    let text = description.map(js_trim).unwrap_or("");
    if text.is_empty() {
        return String::new();
    }
    let escaped = text.replace("*/", "*\\/");
    let lines: Vec<&str> = escaped.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).collect();
    if lines.len() == 1 {
        return format!("{indent}/** {} */\n", lines[0]);
    }
    let body: Vec<String> = lines
        .iter()
        .map(|line| if line.is_empty() { format!("{indent} *") } else { format!("{indent} * {line}") })
        .collect();
    format!("{indent}/**\n{}\n{indent} */\n", body.join("\n"))
}
