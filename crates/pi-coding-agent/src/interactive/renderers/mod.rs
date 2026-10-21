//! Tool call and result renderers (upstream `core/tools/renderers/*` and `render-utils.ts`).
//! A renderer draws a tool's call header and result; it never executes the tool.

mod edit;
mod extras;
mod read;
mod search;
mod shell;
mod timer;
mod write;

use std::any::Any;
use std::path::Path;
use std::rc::Rc;

use pi_ai::ContentBlock;
use pi_tui::terminal_image::{get_capabilities, get_image_dimensions, hyperlink, image_fallback, path_to_file_url};
use pi_tui::{ComponentRef, Text, component};
use serde_json::Value;

use crate::agent_session::bash::{sanitize_binary_output, strip_ansi};
use crate::config::home_dir;
use crate::theme::{ThemeColor, theme};
use crate::tools::path_utils::resolve_to_cwd;

pub use edit::EditRenderer;
pub use extras::{CodemodeRenderer, McpRenderer};
pub use read::ReadRenderer;
pub use search::{FindRenderer, GrepRenderer, LsRenderer};
pub use shell::ShellRenderer;
pub use write::WriteRenderer;

/// A tool result as the renderers see it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolResultView {
    pub content: Vec<ContentBlock>,
    pub details: Option<Value>,
    pub is_error: bool,
    /// Execution time of a final result.
    pub duration_ms: Option<u64>,
}

impl ToolResultView {
    pub fn detail(&self, key: &str) -> Option<&Value> {
        self.details.as_ref()?.get(key)
    }

    /// The text blocks joined with newlines, as written by the tool.
    pub fn raw_text(&self) -> String {
        self.content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n")
    }
}

/// Per-call state a renderer keeps between updates (upstream `context.state`).
#[derive(Default)]
pub struct RendererState(Option<Box<dyn Any>>);

impl RendererState {
    pub fn get_or_default<T: Default + 'static>(&mut self) -> &mut T {
        if !self.0.as_ref().is_some_and(|state| state.is::<T>()) {
            self.0 = Some(Box::new(T::default()));
        }
        self.0.as_mut().and_then(|state| state.downcast_mut::<T>()).expect("state was just set")
    }
}

/// What a renderer knows about the call (upstream `ToolRenderContext`).
pub struct ToolRenderContext<'a> {
    pub args: &'a Value,
    pub tool_call_id: &'a str,
    /// Rebuild this tool's display on the next render, for example after a timer tick.
    pub invalidate: Rc<dyn Fn()>,
    pub state: &'a mut RendererState,
    pub cwd: &'a Path,
    pub execution_started: bool,
    pub args_complete: bool,
    pub is_partial: bool,
    pub expanded: bool,
    pub show_images: bool,
    pub is_error: bool,
    pub duration_ms: Option<u64>,
    pub output_pad: usize,
}

/// Draws a tool's call header and result.
pub trait ToolRenderer {
    fn render_call(&self, context: &mut ToolRenderContext<'_>) -> ComponentRef;
    /// `None` when the result adds nothing below the call header.
    fn render_result(&self, result: &ToolResultView, context: &mut ToolRenderContext<'_>) -> Option<ComponentRef>;

    /// Draw the call header again after the result, because the result changed what it shows.
    fn refresh_call_after_result(&self) -> bool {
        false
    }

    /// `renderShell: "self"`: the renderer draws its own frame, without the default box.
    fn self_shell(&self) -> bool {
        false
    }
}

/// The renderer of a built-in tool (upstream `createAllToolRenderers`).
pub fn builtin_renderer(tool_name: &str) -> Option<Rc<dyn ToolRenderer>> {
    let renderer: Rc<dyn ToolRenderer> = match tool_name {
        "read" => Rc::new(ReadRenderer),
        "bash" => Rc::new(ShellRenderer { prompt: "$" }),
        "powershell" => Rc::new(ShellRenderer { prompt: "PS>" }),
        "edit" => Rc::new(EditRenderer),
        "write" => Rc::new(WriteRenderer),
        "grep" => Rc::new(GrepRenderer),
        "find" => Rc::new(FindRenderer),
        "ls" => Rc::new(LsRenderer),
        "codemode" => Rc::new(CodemodeRenderer),
        _ => return None,
    };
    Some(renderer)
}

/// A text component, the building block of most renderers.
pub fn text(content: &str) -> ComponentRef {
    component(Text::new(content, 0, 0))
}

/// Replace the home directory with `~`.
pub fn shorten_path(path: &str) -> String {
    let home = home_dir().display().to_string();
    match path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// An OSC 8 link to the file when the terminal supports hyperlinks.
pub fn link_path(styled: &str, raw_path: &str, cwd: &Path) -> String {
    if !get_capabilities().hyperlinks {
        return styled.to_string();
    }
    let absolute = resolve_to_cwd(raw_path, cwd);
    hyperlink(styled, &path_to_file_url(&absolute.display().to_string()))
}

/// Upstream `str`: a string argument, `""` when missing, `None` when it has another type.
pub fn str_arg(args: &Value, key: &str) -> Option<String> {
    match args.get(key) {
        None | Some(Value::Null) => Some(String::new()),
        Some(Value::String(value)) => Some(value.clone()),
        Some(_) => None,
    }
}

/// `file_path` when present, else `path` (models send either).
pub fn path_arg(args: &Value) -> Option<String> {
    match args.get("file_path") {
        Some(value) if !value.is_null() => str_arg(args, "file_path"),
        _ => str_arg(args, "path"),
    }
}

pub fn replace_tabs(text: &str) -> String {
    text.replace('\t', "   ")
}

pub fn normalize_display_text(text: &str) -> String {
    text.replace('\r', "")
}

/// Text output of a result; images become fallback lines when they cannot be shown inline.
pub fn get_text_output(result: &ToolResultView, show_images: bool) -> String {
    let mut output = result
        .content
        .iter()
        .filter_map(ContentBlock::as_text)
        .map(|text| sanitize_binary_output(&strip_ansi(text)).replace('\r', ""))
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<String> = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image { data, mime_type } => {
                Some(image_fallback(mime_type, get_image_dimensions(data, mime_type), None))
            }
            _ => None,
        })
        .collect();
    if !images.is_empty() && (get_capabilities().images.is_none() || !show_images) {
        let indicators = images.join("\n");
        output = if output.is_empty() { indicators } else { format!("{output}\n{indicators}") };
    }
    output
}

const COLLAPSED_ARGS_CHARS: usize = 100;

fn json_text(value: &Value, pretty: bool) -> String {
    let text = if pretty { serde_json::to_string_pretty(value) } else { serde_json::to_string(value) };
    text.unwrap_or_default()
}

/// Generic call header: the bold title and the arguments, as `key=value` pairs on the title
/// line when collapsed or one `key: value` line each when expanded.
pub fn format_tool_call_with_args(title: &str, args: &Value, expanded: bool) -> String {
    let theme = theme();
    let header = theme.fg(ThemeColor::ToolTitle, &theme.bold(title));
    let entries: Vec<(String, &Value)> = match args {
        Value::Null => return header,
        Value::Object(map) => map.iter().map(|(key, value)| (key.clone(), value)).collect(),
        other => vec![("args".to_string(), other)],
    };
    if entries.is_empty() {
        return header;
    }
    if expanded {
        let lines: Vec<String> = entries
            .iter()
            .map(|(key, value)| {
                let text = match value {
                    Value::String(text) => text.clone(),
                    other => json_text(other, true),
                };
                let text = replace_tabs(&text).replace('\r', "");
                format!("  {key}: {}", text.split('\n').collect::<Vec<_>>().join("\n    "))
            })
            .collect();
        return format!("{header}\n{}", theme.fg(ThemeColor::Muted, &lines.join("\n")));
    }
    let pairs =
        entries.iter().map(|(key, value)| format!("{key}={}", json_text(value, false))).collect::<Vec<_>>().join(" ");
    let preview = if pairs.encode_utf16().count() > COLLAPSED_ARGS_CHARS {
        let cut: String = pairs.chars().take(COLLAPSED_ARGS_CHARS - 3).collect();
        format!("{cut}...")
    } else {
        pairs
    };
    format!("{header} {}", theme.fg(ThemeColor::Muted, &preview))
}

pub fn invalid_arg_text() -> String {
    theme().fg(ThemeColor::Error, "[invalid arg]")
}

/// The accent path of a call header, linked to the file; `...` while the path streams in.
pub fn render_tool_path(raw_path: Option<&str>, cwd: &Path, empty_fallback: Option<&str>) -> String {
    let Some(raw_path) = raw_path else { return invalid_arg_text() };
    let value = if raw_path.is_empty() { empty_fallback.unwrap_or("") } else { raw_path };
    if value.is_empty() {
        return theme().fg(ThemeColor::ToolOutput, "...");
    }
    link_path(&theme().fg(ThemeColor::Accent, &shorten_path(value)), value, cwd)
}

/// `... (N more lines, <key> to expand)` after a cut listing.
pub fn more_lines_hint(remaining: usize, extra: &str) -> String {
    let theme = theme();
    format!(
        "{} {}{}",
        theme.fg(ThemeColor::Muted, &format!("\n... ({remaining} more lines{extra},")),
        crate::interactive_components::key_hint("app.tools.expand", "to expand"),
        theme.fg(ThemeColor::Muted, ")")
    )
}

pub fn trim_trailing_empty_lines(mut lines: Vec<String>) -> Vec<String> {
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests;
