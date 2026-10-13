//! Pre-rendered HTML for custom tool calls and results (upstream `tool-renderer.ts` and
//! `preRenderCustomTools`). Built-in tools are rendered by the template itself.

use serde::Serialize;
use serde_json::Value;

use super::ansi::ansi_lines_to_html;

/// Tools the HTML template renders directly.
pub const TEMPLATE_RENDERED_TOOLS: [&str; 5] = ["bash", "read", "write", "edit", "ls"];

/// Collapsed and expanded HTML for a tool result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedResultHtml {
    pub collapsed: Option<String>,
    pub expanded: Option<String>,
}

/// Renders custom tools to HTML; `None` means the tool has no custom renderer.
pub trait ToolHtmlRenderer {
    fn render_call(&self, tool_call_id: &str, tool_name: &str, args: &Value) -> Option<String>;
    fn render_result(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        content: &[Value],
        details: &Value,
        is_error: bool,
    ) -> Option<RenderedResultHtml>;
}

/// The `renderedTools[toolCallId]` record embedded in the export.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedToolHtml {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_html_collapsed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_html_expanded: Option<String>,
}

/// Rendered records keyed by tool call id, in first-insertion order.
pub type RenderedTools = Vec<(String, RenderedToolHtml)>;

/// Run the renderer over every custom tool call and result in the session entries.
pub fn pre_render_custom_tools(entries: &[Value], renderer: &dyn ToolHtmlRenderer) -> RenderedTools {
    let mut rendered = RenderedTools::new();
    for entry in entries {
        if entry.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(message) = entry.get("message") else { continue };
        let role = message.get("role").and_then(Value::as_str);
        if role == Some("assistant") {
            for block in message.get("content").and_then(Value::as_array).into_iter().flatten() {
                let name = block.get("name").and_then(Value::as_str).unwrap_or_default();
                if block.get("type").and_then(Value::as_str) != Some("toolCall") || is_template_tool(name) {
                    continue;
                }
                let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                let args = block.get("arguments").unwrap_or(&Value::Null);
                if let Some(call_html) = renderer.render_call(id, name, args).filter(|html| !html.is_empty()) {
                    let record = RenderedToolHtml { call_html: Some(call_html), ..Default::default() };
                    insert(&mut rendered, id, record);
                }
            }
        }
        let tool_call_id = message.get("toolCallId").and_then(Value::as_str).unwrap_or_default();
        if role != Some("toolResult") || tool_call_id.is_empty() {
            continue;
        }
        let tool_name = message.get("toolName").and_then(Value::as_str).unwrap_or_default();
        let existing = rendered.iter().find(|(id, _)| id == tool_call_id).map(|(_, record)| record.clone());
        if existing.is_none() && is_template_tool(tool_name) {
            continue;
        }
        let content = message.get("content").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
        let details = message.get("details").unwrap_or(&Value::Null);
        let is_error = message.get("isError").and_then(Value::as_bool).unwrap_or(false);
        if let Some(result) = renderer.render_result(tool_call_id, tool_name, content, details, is_error) {
            let record = RenderedToolHtml {
                result_html_collapsed: result.collapsed,
                result_html_expanded: result.expanded,
                ..existing.unwrap_or_default()
            };
            insert(&mut rendered, tool_call_id, record);
        }
    }
    rendered
}

fn is_template_tool(name: &str) -> bool {
    TEMPLATE_RENDERED_TOOLS.contains(&name)
}

fn insert(rendered: &mut RenderedTools, id: &str, record: RenderedToolHtml) {
    match rendered.iter_mut().find(|(key, _)| key == id) {
        Some((_, slot)) => *slot = record,
        None => rendered.push((id.to_string(), record)),
    }
}

/// Renders tool calls and results as ANSI lines, the way the TUI draws them.
pub trait AnsiToolRenderer {
    fn render_call_lines(&self, tool_call_id: &str, tool_name: &str, args: &Value, width: usize)
    -> Option<Vec<String>>;
    #[allow(clippy::too_many_arguments)]
    fn render_result_lines(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        content: &[Value],
        details: &Value,
        is_error: bool,
        expanded: bool,
        width: usize,
    ) -> Option<Vec<String>>;
}

/// Adapts an [`AnsiToolRenderer`] to HTML (upstream `createToolHtmlRenderer`).
pub struct AnsiToolRendererAdapter<R> {
    pub renderer: R,
    /// Terminal width for rendering (upstream default 100).
    pub width: usize,
}

impl<R: AnsiToolRenderer> AnsiToolRendererAdapter<R> {
    pub fn new(renderer: R) -> Self {
        Self { renderer, width: 100 }
    }
}

fn is_blank_rendered_line(line: &str) -> bool {
    super::html::strip_sgr(line).trim().is_empty()
}

fn trim_rendered_result_lines(lines: &[String]) -> &[String] {
    let start = lines.iter().position(|line| !is_blank_rendered_line(line)).unwrap_or(lines.len());
    let end = lines.iter().rposition(|line| !is_blank_rendered_line(line)).map_or(start, |i| i + 1);
    &lines[start..end.max(start)]
}

impl<R: AnsiToolRenderer> ToolHtmlRenderer for AnsiToolRendererAdapter<R> {
    fn render_call(&self, tool_call_id: &str, tool_name: &str, args: &Value) -> Option<String> {
        let lines = self.renderer.render_call_lines(tool_call_id, tool_name, args, self.width)?;
        Some(ansi_lines_to_html(&lines))
    }

    fn render_result(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        content: &[Value],
        details: &Value,
        is_error: bool,
    ) -> Option<RenderedResultHtml> {
        let render = |expanded: bool| {
            let lines = self.renderer.render_result_lines(
                tool_call_id,
                tool_name,
                content,
                details,
                is_error,
                expanded,
                self.width,
            )?;
            Some(ansi_lines_to_html(trim_rendered_result_lines(&lines)))
        };
        let collapsed = render(false)?;
        let expanded = render(true)?;
        let collapsed = (!collapsed.is_empty() && collapsed != expanded).then_some(collapsed);
        Some(RenderedResultHtml { collapsed, expanded: Some(expanded) })
    }
}
