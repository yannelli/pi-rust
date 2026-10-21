//! Self-contained HTML export (upstream `core/export-html/index.ts`). The template, its CSS and
//! JavaScript, and the vendored marked, marked-cjk-friendly, and highlight.js run in the
//! viewer's browser.

use std::path::{Path, PathBuf};

use anyhow::bail;
use base64::Engine;
use serde_json::{Map, Value};

use super::json::stringify;
use super::session_file::SessionSnapshot;
use super::theme::ExportTheme;
use super::tool_renderer::{RenderedTools, ToolHtmlRenderer, pre_render_custom_tools};

pub const APP_NAME: &str = "pi";

const TEMPLATE_HTML: &str = include_str!("assets/template.html");
const TEMPLATE_CSS: &str = include_str!("assets/template.css");
const TEMPLATE_JS: &str = include_str!("assets/template.js");
const MARKED_JS: &str = include_str!("assets/vendor/marked.min.js");
const MARKED_CJK_FRIENDLY_JS: &str = include_str!("assets/vendor/marked-cjk-friendly.min.js");
const HIGHLIGHT_JS: &str = include_str!("assets/vendor/highlight.min.js");

/// A tool definition shown in the export header (`name`, `description`, `parameters`).
#[derive(Clone, Debug, PartialEq)]
pub struct ExportTool {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Value,
}

impl ExportTool {
    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("name".into(), self.name.clone().into());
        if let Some(description) = &self.description {
            map.insert("description".into(), description.clone().into());
        }
        map.insert("parameters".into(), self.parameters.clone());
        Value::Object(map)
    }
}

/// Options for [`export_session_html`]. Without `system_prompt` and `tools` the export
/// matches upstream `exportFromFile`; with them it matches `AgentSession.exportToHtml`.
#[derive(Default)]
pub struct HtmlExportOptions<'a> {
    pub theme: ExportTheme,
    pub system_prompt: Option<String>,
    pub tools: Option<Vec<ExportTool>>,
    /// Renders custom (non-template) tools; results are embedded as `renderedTools`.
    pub tool_renderer: Option<&'a dyn ToolHtmlRenderer>,
}

/// The JSON object embedded (base64) in the page.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionData {
    pub header: Map<String, Value>,
    pub entries: Vec<Value>,
    pub leaf_id: Option<Value>,
    pub system_prompt: Option<String>,
    pub tools: Option<Vec<ExportTool>>,
    pub rendered_tools: Option<RenderedTools>,
}

impl SessionData {
    pub fn from_snapshot(snapshot: SessionSnapshot) -> Self {
        Self { header: snapshot.header, entries: snapshot.entries, leaf_id: snapshot.leaf_id, ..Self::default() }
    }

    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("header".into(), Value::Object(self.header.clone()));
        map.insert("entries".into(), Value::Array(self.entries.clone()));
        if let Some(leaf_id) = &self.leaf_id {
            map.insert("leafId".into(), leaf_id.clone());
        }
        if let Some(prompt) = &self.system_prompt {
            map.insert("systemPrompt".into(), prompt.clone().into());
        }
        if let Some(tools) = &self.tools {
            map.insert("tools".into(), tools.iter().map(ExportTool::to_json).collect());
        }
        if let Some(rendered) = self.rendered_tools.as_ref().filter(|rendered| !rendered.is_empty()) {
            let records = rendered
                .iter()
                .map(|(id, record)| (id.clone(), serde_json::to_value(record).unwrap_or_default()))
                .collect();
            map.insert("renderedTools".into(), Value::Object(records));
        }
        Value::Object(map)
    }
}

/// JavaScript `String.prototype.replace` with a string pattern: first match only, and
/// `$$`, `$&`, `` $` ``, `$'` in the replacement are expanded. Upstream relies on it, so
/// the vendored scripts come out with those sequences rewritten.
pub fn js_replace_first(haystack: &str, pattern: &str, replacement: &str) -> String {
    let Some(position) = haystack.find(pattern) else { return haystack.to_string() };
    let (before, after) = (&haystack[..position], &haystack[position + pattern.len()..]);
    let mut out = String::with_capacity(haystack.len() + replacement.len());
    out.push_str(before);
    let mut rest = replacement;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let tail = &rest[dollar + 1..];
        let (expansion, skip) = match tail.as_bytes().first() {
            Some(b'$') => ("$", 2),
            Some(b'&') => (pattern, 2),
            Some(b'`') => (before, 2),
            Some(b'\'') => (after, 2),
            _ => ("$", 1),
        };
        out.push_str(expansion);
        rest = &rest[dollar + skip..];
    }
    out.push_str(rest);
    out.push_str(after);
    out
}

/// Build the HTML page for the session data (upstream `generateHtml`).
pub fn generate_html(data: &SessionData, theme: &ExportTheme) -> String {
    let [body_bg, container_bg, info_bg] = theme.backgrounds();
    let session_json = stringify(&data.to_json());
    let session_base64 = base64::engine::general_purpose::STANDARD.encode(session_json.as_bytes());

    let mut css = js_replace_first(TEMPLATE_CSS, "{{THEME_VARS}}", &theme.css_vars());
    css = js_replace_first(&css, "{{BODY_BG}}", &body_bg);
    css = js_replace_first(&css, "{{CONTAINER_BG}}", &container_bg);
    css = js_replace_first(&css, "{{INFO_BG}}", &info_bg);

    let mut html = js_replace_first(TEMPLATE_HTML, "{{CSS}}", &css);
    html = js_replace_first(&html, "{{JS}}", TEMPLATE_JS);
    html = js_replace_first(&html, "{{SESSION_DATA}}", &session_base64);
    html = js_replace_first(&html, "{{MARKED_JS}}", MARKED_JS);
    html = js_replace_first(&html, "{{MARKED_CJK_FRIENDLY_JS}}", MARKED_CJK_FRIENDLY_JS);
    js_replace_first(&html, "{{HIGHLIGHT_JS}}", HIGHLIGHT_JS)
}

/// Remove `ESC [ params m` sequences.
pub(crate) fn strip_sgr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(esc) = rest.find("\x1b[") {
        out.push_str(&rest[..esc]);
        let after = &rest[esc + 2..];
        let params = after.bytes().take_while(|b| b.is_ascii_digit() || *b == b';').count();
        if after.as_bytes().get(params) == Some(&b'm') {
            rest = &after[params + 1..];
        } else {
            out.push('\x1b');
            rest = &rest[esc + 1..];
        }
    }
    out.push_str(rest);
    out
}

/// `pi-session-<session file name without .jsonl>.html`, relative to the process directory.
pub fn default_html_output_path(session_file: &Path) -> PathBuf {
    let name = session_file.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = name.strip_suffix(".jsonl").filter(|stem| !stem.is_empty()).unwrap_or(&name);
    PathBuf::from(format!("{APP_NAME}-session-{stem}.html"))
}

fn write_html(
    data: &SessionData,
    theme: &ExportTheme,
    session_file: &Path,
    output: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let html = generate_html(data, theme);
    let output_path = match output {
        Some(path) => crate::config::expand_tilde(path),
        None => default_html_output_path(session_file),
    };
    std::fs::write(&output_path, html)?;
    Ok(output_path)
}

/// Export the session file to HTML for `/export` and RPC `export_html`. Returns the
/// written path (`output_path` with `~` expanded, or [`default_html_output_path`]).
pub fn export_session_html(
    session_file: &Path,
    output_path: Option<&str>,
    options: &HtmlExportOptions<'_>,
) -> anyhow::Result<PathBuf> {
    if !session_file.exists() {
        bail!("Nothing to export yet - start a conversation first");
    }
    let snapshot = SessionSnapshot::load(session_file)?;
    let rendered_tools = options
        .tool_renderer
        .map(|renderer| pre_render_custom_tools(&snapshot.entries, renderer))
        .filter(|rendered| !rendered.is_empty());
    let data = SessionData {
        system_prompt: options.system_prompt.clone(),
        tools: options.tools.clone(),
        rendered_tools,
        ..SessionData::from_snapshot(snapshot)
    };
    write_html(&data, &options.theme, session_file, output_path)
}

/// `pi --export <input> [output]`: export any session file (upstream `exportFromFile`).
pub fn export_from_file(input: &str, output_path: Option<&str>, theme: &ExportTheme) -> anyhow::Result<PathBuf> {
    let resolved = resolve_export_input(input)?;
    let data = SessionData::from_snapshot(SessionSnapshot::load(&resolved)?);
    write_html(&data, theme, &resolved, output_path)
}

/// The `--export` input with `~` expanded, against the process directory; it must exist.
pub(crate) fn resolve_export_input(input: &str) -> anyhow::Result<PathBuf> {
    let expanded = crate::config::expand_tilde(input);
    let resolved = if expanded.is_absolute() { expanded } else { std::env::current_dir()?.join(expanded) };
    if !resolved.exists() {
        bail!("File not found: {}", resolved.display());
    }
    Ok(resolved)
}
