//! The write tool renderer (upstream `renderers/write.ts`): the file content as it streams in,
//! highlighted incrementally so long files stay cheap to redraw.

use pi_tui::{ComponentRef, Container, component};
use serde_json::Value;

use super::{
    ToolRenderContext, ToolRenderer, ToolResultView, more_lines_hint, normalize_display_text, path_arg,
    render_tool_path, replace_tabs, str_arg, text, trim_trailing_empty_lines,
};
use crate::theme::{ThemeColor, get_language_from_path, highlight_code, theme};

/// Lines re-highlighted together while streaming, so multi-line constructs near the top settle.
const PARTIAL_FULL_HIGHLIGHT_LINES: usize = 50;

pub struct WriteRenderer;

#[derive(Clone, Default)]
struct HighlightCache {
    raw_path: Option<String>,
    lang: &'static str,
    raw_content: String,
    normalized_lines: Vec<String>,
    highlighted_lines: Vec<String>,
}

#[derive(Default)]
struct WriteState {
    cache: Option<HighlightCache>,
}

fn highlight_line(line: &str, lang: &str) -> String {
    highlight_code(line, Some(lang)).into_iter().next().unwrap_or_default()
}

fn rebuild_full(raw_path: Option<&str>, content: &str) -> Option<HighlightCache> {
    let lang = raw_path.and_then(get_language_from_path)?;
    let normalized = replace_tabs(&normalize_display_text(content));
    Some(HighlightCache {
        raw_path: raw_path.map(String::from),
        lang,
        raw_content: content.to_string(),
        normalized_lines: normalized.split('\n').map(String::from).collect(),
        highlighted_lines: highlight_code(&normalized, Some(lang)),
    })
}

fn refresh_prefix(cache: &mut HighlightCache) {
    let count = PARTIAL_FULL_HIGHLIGHT_LINES.min(cache.normalized_lines.len());
    if count == 0 {
        return;
    }
    let highlighted = highlight_code(&cache.normalized_lines[..count].join("\n"), Some(cache.lang));
    for index in 0..count {
        cache.highlighted_lines[index] = highlighted
            .get(index)
            .cloned()
            .unwrap_or_else(|| highlight_line(&cache.normalized_lines[index], cache.lang));
    }
}

fn update_incremental(cache: Option<HighlightCache>, raw_path: Option<&str>, content: &str) -> Option<HighlightCache> {
    let lang = raw_path.and_then(get_language_from_path)?;
    let Some(mut cache) = cache else { return rebuild_full(raw_path, content) };
    if cache.lang != lang || cache.raw_path.as_deref() != raw_path || !content.starts_with(&cache.raw_content) {
        return rebuild_full(raw_path, content);
    }
    if content.len() == cache.raw_content.len() {
        return Some(cache);
    }
    let delta = replace_tabs(&normalize_display_text(&content[cache.raw_content.len()..]));
    cache.raw_content = content.to_string();
    if cache.normalized_lines.is_empty() {
        cache.normalized_lines.push(String::new());
        cache.highlighted_lines.push(String::new());
    }
    let mut segments = delta.split('\n');
    let last = cache.normalized_lines.len() - 1;
    cache.normalized_lines[last].push_str(segments.next().unwrap_or(""));
    cache.highlighted_lines.resize(cache.normalized_lines.len(), String::new());
    cache.highlighted_lines[last] = highlight_line(&cache.normalized_lines[last], lang);
    for segment in segments {
        cache.normalized_lines.push(segment.to_string());
        cache.highlighted_lines.push(highlight_line(segment, lang));
    }
    refresh_prefix(&mut cache);
    Some(cache)
}

fn format_write_call(
    args: &Value,
    expanded: bool,
    cache: Option<&HighlightCache>,
    context: &ToolRenderContext<'_>,
) -> String {
    let theme = theme();
    let raw_path = path_arg(args);
    let path = render_tool_path(raw_path.as_deref(), context.cwd, None);
    let mut text = format!("{} {path}", theme.fg(ThemeColor::ToolTitle, &theme.bold("write")));
    match str_arg(args, "content") {
        None => {
            let error = theme.fg(ThemeColor::Error, "[invalid content arg - expected string]");
            text.push_str(&format!("\n\n{error}"));
        }
        Some(content) if !content.is_empty() => {
            let lang = raw_path.as_deref().and_then(get_language_from_path);
            let rendered = match (lang, cache) {
                (Some(_), Some(cache)) => cache.highlighted_lines.clone(),
                (Some(lang), None) => highlight_code(&replace_tabs(&normalize_display_text(&content)), Some(lang)),
                (None, _) => normalize_display_text(&content).split('\n').map(String::from).collect(),
            };
            let lines = trim_trailing_empty_lines(rendered);
            let total = lines.len();
            let max = if expanded { total } else { 10 };
            let shown: Vec<String> =
                lines
                    .iter()
                    .take(max)
                    .map(|line| {
                        if lang.is_some() {
                            line.clone()
                        } else {
                            theme.fg(ThemeColor::ToolOutput, &replace_tabs(line))
                        }
                    })
                    .collect();
            text.push_str(&format!("\n\n{}", shown.join("\n")));
            if total > max {
                text.push_str(&more_lines_hint(total - max, &format!(", {total} total")));
            }
        }
        Some(_) => {}
    }
    text
}

impl ToolRenderer for WriteRenderer {
    fn render_call(&self, context: &mut ToolRenderContext<'_>) -> ComponentRef {
        let raw_path = path_arg(context.args);
        let content = str_arg(context.args, "content");
        let args_complete = context.args_complete;
        let state = context.state.get_or_default::<WriteState>();
        state.cache = match content {
            Some(content) if args_complete => rebuild_full(raw_path.as_deref(), &content),
            Some(content) => update_incremental(state.cache.take(), raw_path.as_deref(), &content),
            None => None,
        };
        let cache = state.cache.clone();
        text(&format_write_call(context.args, context.expanded, cache.as_ref(), context))
    }

    fn render_result(&self, result: &ToolResultView, context: &mut ToolRenderContext<'_>) -> Option<ComponentRef> {
        let output = result.raw_text();
        if !context.is_error || output.is_empty() {
            return Some(component(Container::new()));
        }
        Some(text(&format!("\n{}", theme().fg(ThemeColor::Error, &output))))
    }
}
