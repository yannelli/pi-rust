//! The read tool renderer (upstream `renderers/read.ts`): a compact header for skills and
//! context files, highlighted file content when expanded, and truncation notes.

use std::path::Path;

use pi_tui::ComponentRef;
use serde_json::Value;

use super::{
    ToolRenderContext, ToolRenderer, ToolResultView, get_text_output, more_lines_hint, path_arg, render_tool_path,
    replace_tabs, text, trim_trailing_empty_lines,
};
use crate::interactive::util::format_path_relative_to_cwd_or_absolute;
use crate::interactive_components::key_text;
use crate::theme::{ThemeColor, get_language_from_path, highlight_code, theme};
use crate::tools::path_utils::resolve_to_cwd;
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size};

const COMPACT_RESOURCE_FILE_NAMES: [&str; 5] =
    ["AGENTS.override.md", "AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];

pub struct ReadRenderer;

enum Compact {
    Skill(String),
    Resource(String),
}

fn line_range(args: &Value) -> String {
    let offset = args.get("offset").and_then(Value::as_i64);
    let limit = args.get("limit").and_then(Value::as_i64);
    if offset.is_none() && limit.is_none() {
        return String::new();
    }
    let start = offset.unwrap_or(1);
    let end = limit.map(|limit| format!("-{}", start + limit - 1)).unwrap_or_default();
    theme().fg(ThemeColor::Warning, &format!(":{start}{end}"))
}

fn classify(args: &Value, cwd: &Path) -> Option<Compact> {
    let raw = path_arg(args).filter(|path| !path.is_empty())?;
    let absolute = resolve_to_cwd(&raw, cwd);
    let file_name = absolute.file_name()?.to_string_lossy().to_string();
    if file_name == "SKILL.md" {
        let parent = absolute.parent().and_then(Path::file_name).map(|name| name.to_string_lossy().to_string());
        return Some(Compact::Skill(parent.filter(|name| !name.is_empty()).unwrap_or(file_name)));
    }
    COMPACT_RESOURCE_FILE_NAMES
        .contains(&file_name.as_str())
        .then(|| Compact::Resource(format_path_relative_to_cwd_or_absolute(&absolute, cwd)))
}

fn format_compact_call(compact: &Compact, args: &Value) -> String {
    let theme = theme();
    let hint = theme.fg(ThemeColor::Dim, &format!(" ({} to expand)", key_text("app.tools.expand")));
    match compact {
        Compact::Skill(label) => {
            theme.fg(ThemeColor::CustomMessageLabel, "\x1b[1m[skill]\x1b[22m ")
                + &theme.fg(ThemeColor::CustomMessageText, label)
                + &line_range(args)
                + &hint
        }
        Compact::Resource(label) => {
            theme.fg(ThemeColor::ToolTitle, &theme.bold("read resource"))
                + " "
                + &theme.fg(ThemeColor::Accent, label)
                + &line_range(args)
                + &hint
        }
    }
}

fn format_read_call(args: &Value, cwd: &Path) -> String {
    let theme = theme();
    let path = render_tool_path(path_arg(args).as_deref(), cwd, None);
    format!("{} {path}{}", theme.fg(ThemeColor::ToolTitle, &theme.bold("read")), line_range(args))
}

fn number(value: Option<&Value>, key: &str) -> Option<u64> {
    value?.get(key)?.as_u64()
}

fn format_read_result(result: &ToolResultView, context: &ToolRenderContext<'_>) -> String {
    if !context.expanded && !context.is_error {
        return String::new();
    }
    let theme = theme();
    let raw_path = path_arg(context.args);
    let output = get_text_output(result, context.show_images);
    let lang = raw_path.as_deref().filter(|_| !context.is_error).and_then(get_language_from_path);
    let rendered = match lang {
        Some(lang) => highlight_code(&replace_tabs(&output), Some(lang)),
        None => output.split('\n').map(String::from).collect(),
    };
    let lines = trim_trailing_empty_lines(rendered);
    let max_lines = if context.expanded { lines.len() } else { 10 };
    let shown: Vec<String> =
        lines
            .iter()
            .take(max_lines)
            .map(|line| {
                if lang.is_some() { replace_tabs(line) } else { theme.fg(ThemeColor::ToolOutput, &replace_tabs(line)) }
            })
            .collect();
    let mut text = format!("\n{}", shown.join("\n"));
    if lines.len() > max_lines {
        text.push_str(&more_lines_hint(lines.len() - max_lines, ""));
    }
    let truncation = result.detail("truncation");
    if truncation.and_then(|t| t.get("truncated")).and_then(Value::as_bool) == Some(true) {
        let max_bytes = format_size(number(truncation, "maxBytes").map_or(DEFAULT_MAX_BYTES, |bytes| bytes as usize));
        let output_lines = number(truncation, "outputLines").unwrap_or(0);
        let warning = if truncation.and_then(|t| t.get("firstLineExceedsLimit")).and_then(Value::as_bool) == Some(true)
        {
            format!("[First line exceeds {max_bytes} limit]")
        } else if truncation.and_then(|t| t.get("truncatedBy")).and_then(Value::as_str) == Some("lines") {
            let total = number(truncation, "totalLines").unwrap_or(0);
            let max_lines = number(truncation, "maxLines").unwrap_or(DEFAULT_MAX_LINES as u64);
            format!("[Truncated: showing {output_lines} of {total} lines ({max_lines} line limit)]")
        } else {
            format!("[Truncated: {output_lines} lines shown ({max_bytes} limit)]")
        };
        text.push_str(&format!("\n{}", theme.fg(ThemeColor::Warning, &warning)));
    }
    text
}

impl ToolRenderer for ReadRenderer {
    fn render_call(&self, context: &mut ToolRenderContext<'_>) -> ComponentRef {
        let compact = if context.expanded { None } else { classify(context.args, context.cwd) };
        match compact {
            Some(compact) => text(&format_compact_call(&compact, context.args)),
            None => text(&format_read_call(context.args, context.cwd)),
        }
    }

    fn render_result(&self, result: &ToolResultView, context: &mut ToolRenderContext<'_>) -> Option<ComponentRef> {
        Some(text(&format_read_result(result, context)))
    }
}
