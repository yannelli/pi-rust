//! Renderers of the grep, find, and ls tools (upstream `renderers/grep.ts`, `find.ts`, `ls.ts`).

use pi_tui::ComponentRef;
use serde_json::Value;

use super::{
    ToolRenderContext, ToolRenderer, ToolResultView, get_text_output, invalid_arg_text, more_lines_hint,
    render_tool_path, shorten_path, str_arg, text,
};
use crate::theme::{ThemeColor, theme};
use crate::tools::truncate::{DEFAULT_MAX_BYTES, format_size};

pub struct GrepRenderer;
pub struct FindRenderer;
pub struct LsRenderer;

fn limit_arg(args: &Value) -> Option<String> {
    match args.get("limit") {
        None | Some(Value::Null) => None,
        Some(Value::Number(number)) => Some(number.to_string()),
        Some(other) => Some(other.to_string()),
    }
}

fn search_path(args: &Value) -> String {
    match str_arg(args, "path") {
        None => invalid_arg_text(),
        Some(path) => shorten_path(if path.is_empty() { "." } else { &path }),
    }
}

/// Output lines cut to `collapsed_lines` unless expanded, then a truncation warning.
fn format_listing(
    result: &ToolResultView,
    context: &ToolRenderContext<'_>,
    collapsed_lines: usize,
    limit_key: &str,
    limit_label: &str,
    extra: &[(&str, &str)],
) -> String {
    let theme = theme();
    let output = get_text_output(result, context.show_images).trim().to_string();
    let mut text = String::new();
    if !output.is_empty() {
        let lines: Vec<&str> = output.split('\n').collect();
        let max = if context.expanded { lines.len() } else { collapsed_lines };
        let shown: Vec<String> = lines.iter().take(max).map(|line| theme.fg(ThemeColor::ToolOutput, line)).collect();
        text.push_str(&format!("\n{}", shown.join("\n")));
        if lines.len() > max {
            text.push_str(&more_lines_hint(lines.len() - max, ""));
        }
    }
    let limit = result.detail(limit_key).filter(|value| !value.is_null() && *value != &Value::Bool(false));
    let truncation = result.detail("truncation");
    let truncated = truncation.and_then(|t| t.get("truncated")).and_then(Value::as_bool) == Some(true);
    let flags: Vec<&str> = extra
        .iter()
        .filter(|(key, _)| result.detail(key).and_then(Value::as_bool) == Some(true))
        .map(|(_, label)| *label)
        .collect();
    if limit.is_some() || truncated || !flags.is_empty() {
        let mut warnings = Vec::new();
        if let Some(limit) = limit {
            warnings.push(format!("{limit} {limit_label} limit"));
        }
        if truncated {
            let max = truncation.and_then(|t| t.get("maxBytes")).and_then(Value::as_u64);
            warnings.push(format!("{} limit", format_size(max.map_or(DEFAULT_MAX_BYTES, |bytes| bytes as usize))));
        }
        warnings.extend(flags.iter().map(|flag| flag.to_string()));
        text.push_str(&format!(
            "\n{}",
            theme.fg(ThemeColor::Warning, &format!("[Truncated: {}]", warnings.join(", ")))
        ));
    }
    text
}

impl ToolRenderer for GrepRenderer {
    fn render_call(&self, context: &mut ToolRenderContext<'_>) -> ComponentRef {
        let theme = theme();
        let args = context.args;
        let pattern = match str_arg(args, "pattern") {
            None => invalid_arg_text(),
            Some(pattern) => theme.fg(ThemeColor::Accent, &format!("/{pattern}/")),
        };
        let mut line = theme.fg(ThemeColor::ToolTitle, &theme.bold("grep"))
            + " "
            + &pattern
            + &theme.fg(ThemeColor::ToolOutput, &format!(" in {}", search_path(args)));
        if let Some(glob) = str_arg(args, "glob").filter(|glob| !glob.is_empty()) {
            line.push_str(&theme.fg(ThemeColor::ToolOutput, &format!(" ({glob})")));
        }
        if let Some(limit) = limit_arg(args) {
            line.push_str(&theme.fg(ThemeColor::ToolOutput, &format!(" limit {limit}")));
        }
        text(&line)
    }

    fn render_result(&self, result: &ToolResultView, context: &mut ToolRenderContext<'_>) -> Option<ComponentRef> {
        let lines_truncated = [("linesTruncated", "some lines truncated")];
        Some(text(&format_listing(result, context, 15, "matchLimitReached", "matches", &lines_truncated)))
    }
}

impl ToolRenderer for FindRenderer {
    fn render_call(&self, context: &mut ToolRenderContext<'_>) -> ComponentRef {
        let theme = theme();
        let args = context.args;
        let pattern = match str_arg(args, "pattern") {
            None => invalid_arg_text(),
            Some(pattern) => theme.fg(ThemeColor::Accent, &pattern),
        };
        let mut line = theme.fg(ThemeColor::ToolTitle, &theme.bold("find"))
            + " "
            + &pattern
            + &theme.fg(ThemeColor::ToolOutput, &format!(" in {}", search_path(args)));
        if let Some(limit) = limit_arg(args) {
            line.push_str(&theme.fg(ThemeColor::ToolOutput, &format!(" (limit {limit})")));
        }
        text(&line)
    }

    fn render_result(&self, result: &ToolResultView, context: &mut ToolRenderContext<'_>) -> Option<ComponentRef> {
        Some(text(&format_listing(result, context, 20, "resultLimitReached", "results", &[])))
    }
}

impl ToolRenderer for LsRenderer {
    fn render_call(&self, context: &mut ToolRenderContext<'_>) -> ComponentRef {
        let theme = theme();
        let path = render_tool_path(str_arg(context.args, "path").as_deref(), context.cwd, Some("."));
        let mut line = format!("{} {path}", theme.fg(ThemeColor::ToolTitle, &theme.bold("ls")));
        if let Some(limit) = limit_arg(context.args) {
            line.push_str(&theme.fg(ThemeColor::ToolOutput, &format!(" (limit {limit})")));
        }
        text(&line)
    }

    fn render_result(&self, result: &ToolResultView, context: &mut ToolRenderContext<'_>) -> Option<ComponentRef> {
        Some(text(&format_listing(result, context, 20, "entryLimitReached", "entries", &[])))
    }
}
