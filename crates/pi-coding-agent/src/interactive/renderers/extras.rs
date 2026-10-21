//! Renderers of the built-in extension tools: MCP tools (upstream `extensions/mcp/tools.ts`) and
//! codemode (upstream `extensions/codemode/renderer.ts`).

use pi_codemode::{CodemodeNestedCall, CodemodeNestedCallStatus};
use pi_tui::{ComponentRef, Container, Spacer, component};
use serde_json::Value;

use super::timer::{start_tool_timer, tool_timer_text};
use super::{
    ToolRenderContext, ToolRenderer, ToolResultView, format_tool_call_with_args, get_text_output, invalid_arg_text,
    replace_tabs, str_arg, text,
};
use crate::interactive::components::visual_truncate::{Keep, VisualLinePreview};
use crate::interactive_components::key_hint;
use crate::theme::{ThemeColor, highlight_code, theme};

const OUTPUT_PREVIEW_LINES: usize = 5;
const CODE_PREVIEW_LINES: usize = 10;
const CALL_PREVIEW_COUNT: usize = 8;
const COLLAPSED_ARGS_CHARS: usize = 80;

fn expand_hint(hidden: &str) -> String {
    let theme = theme();
    format!(
        "{} {}{}",
        theme.fg(ThemeColor::Muted, &format!("... ({hidden},")),
        key_hint("app.tools.expand", "to expand"),
        theme.fg(ThemeColor::Muted, ")")
    )
}

/// The output lines below a call: all of them when expanded, else a 5-line visual preview and
/// the `Full output:` path.
fn add_output(container: &mut Container, output: &str, result: &ToolResultView, context: &ToolRenderContext<'_>) {
    let theme = theme();
    let color = if context.is_error { ThemeColor::Error } else { ThemeColor::ToolOutput };
    let styled = replace_tabs(output).split('\n').map(|line| theme.fg(color, line)).collect::<Vec<_>>().join("\n");
    container.add_child(component(Spacer::new(1)));
    if context.expanded {
        container.add_child(text(&styled));
        return;
    }
    let preview = VisualLinePreview::new(styled, OUTPUT_PREVIEW_LINES, Keep::Start, |hidden| {
        expand_hint(&format!("{hidden} more lines"))
    });
    container.add_child(component(preview));
    if let Some(path) = result.detail("fullOutputPath").and_then(Value::as_str) {
        container.add_child(text(&theme.fg(ThemeColor::Muted, &format!("Full output: {path}"))));
    }
}

/// Calls to an MCP tool, labeled `server/tool`.
pub struct McpRenderer {
    pub label: String,
}

impl McpRenderer {
    /// The renderer of a tool named `mcp__<server>__<tool>`, for calls whose server is not connected.
    pub fn from_tool_name(name: &str) -> Option<Self> {
        let rest = name.strip_prefix("mcp__")?;
        let split = rest.get(1..)?.find("__")? + 1;
        let (server, tool) = (&rest[..split], &rest[split + 2..]);
        (!tool.is_empty()).then(|| Self { label: format!("{server}/{tool}") })
    }
}

impl ToolRenderer for McpRenderer {
    fn render_call(&self, context: &mut ToolRenderContext<'_>) -> ComponentRef {
        text(&format_tool_call_with_args(&self.label, context.args, context.expanded))
    }

    fn render_result(&self, result: &ToolResultView, context: &mut ToolRenderContext<'_>) -> Option<ComponentRef> {
        let output = get_text_output(result, context.show_images).trim().to_string();
        if output.is_empty() {
            return None;
        }
        let mut container = Container::new();
        add_output(&mut container, &output, result, context);
        Some(component(container))
    }
}

fn format_ms(ms: Option<f64>) -> String {
    match ms {
        None => String::new(),
        Some(ms) if ms < 1000.0 => format!("{}ms", ms.round()),
        Some(ms) => format!("{:.1}s", ms / 1000.0),
    }
}

/// Cents for larger amounts, two significant digits for fractions of a cent.
pub fn format_cost(cost: f64) -> String {
    if cost >= 0.01 {
        return format!("${cost:.2}");
    }
    let magnitude = cost.abs().log10().floor() as i32;
    let decimals = (1 - magnitude).max(0) as usize;
    format!("${cost:.decimals$}")
}

fn format_nested_call(call: &CodemodeNestedCall, expanded: bool) -> String {
    let theme = theme();
    let icon = match call.status {
        CodemodeNestedCallStatus::Running => theme.fg(ThemeColor::Warning, "…"),
        CodemodeNestedCallStatus::Ok => theme.fg(ThemeColor::Success, "✓"),
        CodemodeNestedCallStatus::Error => theme.fg(ThemeColor::Error, "✗"),
        CodemodeNestedCallStatus::Cancelled => theme.fg(ThemeColor::Muted, "⊘"),
    };
    let args = if !expanded && call.args.encode_utf16().count() > COLLAPSED_ARGS_CHARS {
        format!("{}...", call.args.chars().take(COLLAPSED_ARGS_CHARS - 3).collect::<String>())
    } else {
        call.args.clone()
    };
    let mut line = format!("{icon} {}", theme.fg(ThemeColor::ToolTitle, &call.name));
    if !args.is_empty() {
        line += &format!(" {}", theme.fg(ThemeColor::Muted, &args));
    }
    let duration = format_ms(call.duration_ms);
    if !duration.is_empty() {
        line += &format!(" {}", theme.fg(ThemeColor::Dim, &duration));
    }
    if let Some(cost) = call.cost.filter(|cost| *cost != 0.0) {
        line += &format!(" {}", theme.fg(ThemeColor::Dim, &format_cost(cost)));
    }
    if expanded && let Some(error) = &call.error {
        line +=
            &format!("\n    {}", theme.fg(ThemeColor::Error, &error.split('\n').collect::<Vec<_>>().join("\n    ")));
    }
    line
}

/// `Script completed\nWall time N seconds\nOutput:\n`, the header the renderer drops.
fn is_script_header(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("Script completed\n").or_else(|| text.strip_prefix("Script failed\n")) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix("Wall time ") else { return false };
    let Some((number, tail)) = rest.split_once(' ') else { return false };
    !number.is_empty() && number.chars().all(|c| c.is_ascii_digit() || c == '.') && tail == "seconds\nOutput:\n"
}

/// The codemode tool: the script, its nested calls with status, and the script output.
pub struct CodemodeRenderer;

impl ToolRenderer for CodemodeRenderer {
    fn render_call(&self, context: &mut ToolRenderContext<'_>) -> ComponentRef {
        start_tool_timer(context);
        let theme = theme();
        let title = theme.fg(ThemeColor::ToolTitle, &theme.bold("codemode"));
        let mut container = Container::new();
        let Some(code) = str_arg(context.args, "code") else {
            container.add_child(text(&format!("{title} {}", invalid_arg_text())));
            return component(container);
        };
        container.add_child(text(&title));
        if !code.is_empty() {
            let code = replace_tabs(code.replace('\r', "").trim_end());
            let highlighted = highlight_code(&code, Some("javascript")).join("\n");
            if context.expanded {
                container.add_child(text(&highlighted));
            } else {
                let preview = VisualLinePreview::new(highlighted, CODE_PREVIEW_LINES, Keep::Start, |hidden| {
                    expand_hint(&format!("{hidden} more lines"))
                });
                container.add_child(component(preview));
            }
        }
        component(container)
    }

    fn render_result(&self, result: &ToolResultView, context: &mut ToolRenderContext<'_>) -> Option<ComponentRef> {
        let timer_text = tool_timer_text(context);
        let theme = theme();
        let mut container = Container::new();
        let mut empty = true;
        let calls: Vec<CodemodeNestedCall> =
            result.detail("calls").and_then(|calls| serde_json::from_value(calls.clone()).ok()).unwrap_or_default();
        if !calls.is_empty() {
            let skip = if context.expanded { 0 } else { calls.len().saturating_sub(CALL_PREVIEW_COUNT) };
            let mut lines: Vec<String> =
                calls[skip..].iter().map(|call| format_nested_call(call, context.expanded)).collect();
            if skip > 0 {
                lines.insert(0, expand_hint(&format!("{skip} earlier calls")));
            }
            let priced: Vec<f64> = calls.iter().filter_map(|call| call.cost).filter(|cost| *cost != 0.0).collect();
            if priced.len() > 1 {
                let total: f64 = priced.iter().sum();
                lines.push(theme.fg(ThemeColor::Muted, &format!("Model calls: {}", format_cost(total))));
            }
            container.add_child(component(Spacer::new(1)));
            container.add_child(text(&lines.join("\n")));
            empty = false;
        }
        let output = if context.is_partial {
            String::new()
        } else {
            let has_header = result.content.first().and_then(|block| block.as_text()).is_some_and(is_script_header);
            let view = ToolResultView { content: result.content[usize::from(has_header)..].to_vec(), ..result.clone() };
            get_text_output(&view, context.show_images).trim().to_string()
        };
        if !output.is_empty() {
            add_output(&mut container, &output, result, context);
            empty = false;
        }
        if let Some(timer_text) = timer_text {
            container.add_child(component(Spacer::new(1)));
            container.add_child(text(&theme.fg(ThemeColor::Muted, &timer_text)));
            empty = false;
        }
        (!empty).then(|| component(container) as ComponentRef)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mcp_tool_names() {
        assert_eq!(McpRenderer::from_tool_name("mcp__github__list_issues").unwrap().label, "github/list_issues");
        assert_eq!(McpRenderer::from_tool_name("mcp__a__b__c").unwrap().label, "a/b__c");
        assert!(McpRenderer::from_tool_name("mcp__github").is_none());
        assert!(McpRenderer::from_tool_name("read").is_none());
    }

    #[test]
    fn formats_costs_and_headers() {
        assert_eq!(format_cost(0.1234), "$0.12");
        assert_eq!(format_cost(0.0012345), "$0.0012");
        assert!(is_script_header("Script completed\nWall time 0.5 seconds\nOutput:\n"));
        assert!(is_script_header("Script failed\nWall time 12 seconds\nOutput:\n"));
        assert!(!is_script_header("Script completed\nWall time 0.5 seconds\nOutput:\nhello"));
    }
}
