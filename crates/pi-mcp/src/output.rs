//! Model-facing results of MCP tools. Text over 20 KB keeps its start and end with the middle cut
//! out, and the full text goes to a temp file. Binary resources other than images go to temp files.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Serialize;
use serde_json::Value;

use crate::content::{CallToolResult, ContentBlock, to_llm_content};
use crate::naming::READ_MCP_RESOURCE_TOOL;
use crate::oauth::pkce::random_bytes;

/// Model-facing text of an MCP result beyond this is cut in the middle.
pub const MCP_OUTPUT_MAX_BYTES: usize = 20 * 1024;

/// Saves the full text of a truncated result, or a binary resource, and returns the path.
/// The extension includes the dot, like `.txt`.
pub type OutputSaver = Arc<dyn Fn(&[u8], &str) -> Result<PathBuf, String> + Send + Sync>;

/// Write `data` to a new file only the user can read: `<temp>/pi-mcp-<random hex><extension>`.
pub fn save_to_temp_file(data: &[u8], extension: &str) -> Result<PathBuf, String> {
    let name: String = random_bytes::<8>().iter().map(|byte| format!("{byte:02x}")).collect();
    let path = std::env::temp_dir().join(format!("pi-mcp-{name}{extension}"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path).map_err(|error| error.to_string())?;
    std::io::Write::write_all(&mut file, data).map_err(|error| error.to_string())?;
    Ok(path)
}

pub fn default_saver() -> OutputSaver {
    Arc::new(save_to_temp_file)
}

pub fn format_size(bytes: f64) -> String {
    if bytes < 1024.0 {
        format!("{bytes}B")
    } else if bytes < 1024.0 * 1024.0 {
        format!("{:.1}KB", bytes / 1024.0)
    } else {
        format!("{:.1}MB", bytes / (1024.0 * 1024.0))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MiddleTruncation {
    pub content: String,
    pub truncated: bool,
    pub removed_chars: usize,
    pub total_bytes: usize,
    pub total_lines: usize,
}

/// Keep the first and last `max_bytes / 2` bytes, cutting only at character boundaries.
pub fn truncate_middle(content: &str, max_bytes: usize) -> MiddleTruncation {
    let total_bytes = content.len();
    let total_lines =
        if content.is_empty() { 0 } else { content.split('\n').count() - usize::from(content.ends_with('\n')) };
    if total_bytes <= max_bytes {
        return MiddleTruncation {
            content: content.into(),
            truncated: false,
            removed_chars: 0,
            total_bytes,
            total_lines,
        };
    }
    let mut head_end = max_bytes / 2;
    while head_end > 0 && !content.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = total_bytes - (max_bytes - max_bytes / 2);
    while tail_start < total_bytes && !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let removed_chars = content[head_end..tail_start].chars().count();
    let text = format!("{}…{removed_chars} chars truncated…{}", &content[..head_end], &content[tail_start..]);
    MiddleTruncation { content: text, truncated: true, removed_chars, total_bytes, total_lines }
}

fn text_of(content: &[pi_ai::ContentBlock]) -> String {
    content.iter().filter_map(pi_ai::ContentBlock::as_text).collect::<Vec<_>>().join("\n")
}

/// Keep model-facing text within 20 KB: longer text becomes one text block in Codex's truncation
/// format with the path of the full text; images follow it.
pub fn limit_mcp_content(
    content: Vec<pi_ai::ContentBlock>,
    saver: &OutputSaver,
) -> (Vec<pi_ai::ContentBlock>, Option<String>) {
    let combined = text_of(&content);
    let truncation = truncate_middle(&combined, MCP_OUTPUT_MAX_BYTES);
    if !truncation.truncated {
        return (content, None);
    }
    let (path, location) = match saver(combined.as_bytes(), ".txt") {
        Ok(path) => {
            let path = path.display().to_string();
            let location = format!("[Full output: {path} (read it with offset/limit)]");
            (Some(path), location)
        }
        Err(error) => (None, format!("[Could not save the full output: {error}]")),
    };
    let tokens = truncation.total_bytes.div_ceil(4);
    let text = format!(
        "Warning: truncated output (original token count: {tokens})\nTotal output lines: {}\n\n{}\n\n{location}",
        truncation.total_lines, truncation.content
    );
    let mut limited = vec![pi_ai::ContentBlock::text(text)];
    limited.extend(content.into_iter().filter(|block| matches!(block, pi_ai::ContentBlock::Image { .. })));
    (limited, path)
}

#[derive(Clone)]
pub struct ConvertOptions {
    pub saver: OutputSaver,
    /// Whether `read_mcp_resource` can read the server's resources, which resource links then name.
    pub readable_resources: bool,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self { saver: default_saver(), readable_resources: false }
    }
}

/// Extension for a saved binary resource: the one its URI ends in, else `.bin`.
fn extension_of(uri: &str) -> String {
    let path = url::Url::parse(uri).map(|url| url.path().to_string()).unwrap_or_else(|_| uri.to_string());
    let extension = path.rsplit_once('.').map(|(_, extension)| extension).unwrap_or_default();
    let valid =
        (1..=8).contains(&extension.len()) && extension.chars().all(|character| character.is_ascii_alphanumeric());
    if valid { format!(".{extension}") } else { ".bin".into() }
}

fn is_text_mime_type(mime_type: Option<&str>) -> bool {
    let Some(mime_type) = mime_type else { return false };
    let kind = mime_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    kind.starts_with("text/") || kind == "application/json" || kind.ends_with("+json") || kind.ends_with("+xml")
}

fn block_to_content(server: &str, block: &ContentBlock, options: &ConvertOptions) -> Vec<pi_ai::ContentBlock> {
    match block {
        ContentBlock::ResourceLink { uri, name, title, description, mime_type, size, .. } => {
            let mut details: Vec<String> = mime_type.iter().filter(|mime| !mime.is_empty()).cloned().collect();
            details.extend(size.map(format_size));
            let details = if details.is_empty() { String::new() } else { format!(" ({})", details.join(", ")) };
            let description = description.as_deref().filter(|text| !text.is_empty()).map(|text| format!(": {text}"));
            let read = if options.readable_resources {
                format!(". Read it with {READ_MCP_RESOURCE_TOOL} (server \"{server}\")")
            } else {
                String::new()
            };
            let label = title.as_deref().unwrap_or(name);
            let text = format!("[Resource {uri} \"{label}\"{details}{}{read}]", description.unwrap_or_default());
            vec![pi_ai::ContentBlock::text(text)]
        }
        ContentBlock::Resource { resource, .. }
            if resource.blob.is_some()
                && !resource.mime_type.as_deref().is_some_and(|mime| mime.starts_with("image/")) =>
        {
            let data = STANDARD.decode(resource.blob.as_deref().unwrap_or_default().trim()).unwrap_or_default();
            if is_text_mime_type(resource.mime_type.as_deref()) {
                return vec![pi_ai::ContentBlock::text(String::from_utf8_lossy(&data).into_owned())];
            }
            let kind = format!(
                "{}, {}",
                resource.mime_type.as_deref().unwrap_or("unknown type"),
                format_size(data.len() as f64)
            );
            let text = match (options.saver)(&data, &extension_of(&resource.uri)) {
                Ok(path) => format!("[Binary resource {} ({kind}) saved to {}]", resource.uri, path.display()),
                Err(reason) => format!("[Binary resource {} ({kind}) could not be saved: {reason}]", resource.uri),
            };
            vec![pi_ai::ContentBlock::text(text)]
        }
        block => to_llm_content(std::slice::from_ref(block), None),
    }
}

/// Model-facing content of a server's content blocks, before the output limit.
pub fn to_model_content(server: &str, blocks: &[ContentBlock], options: &ConvertOptions) -> Vec<pi_ai::ContentBlock> {
    blocks.iter().flat_map(|block| block_to_content(server, block, options)).collect()
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDetails {
    pub server: String,
    pub tool: String,
    /// Temp file with the full text, when the model-facing text was truncated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

/// A converted result: model content, details, and the `CallToolResult` (without `_meta`) that
/// codemode scripts receive whole.
#[derive(Clone, Debug, PartialEq)]
pub struct McpToolOutput {
    pub content: Vec<pi_ai::ContentBlock>,
    pub details: McpToolDetails,
    pub structured_content: Value,
    pub is_error: bool,
}

pub fn convert_mcp_result(
    server: &str,
    tool: &str,
    result: &CallToolResult,
    options: &ConvertOptions,
) -> McpToolOutput {
    let mut converted = if result.content.is_empty() {
        to_llm_content(&[], result.structured_content.as_ref())
    } else {
        to_model_content(server, &result.content, options)
    };
    if result.is_error() && text_of(&converted).is_empty() {
        converted.push(pi_ai::ContentBlock::text(format!("MCP tool {server}/{tool} returned an error")));
    }
    let (content, full_output_path) = limit_mcp_content(converted, &options.saver);
    let mut structured_content = serde_json::to_value(result).unwrap_or(Value::Null);
    if let Some(object) = structured_content.as_object_mut() {
        object.shift_remove("_meta");
    }
    let details = McpToolDetails { server: server.into(), tool: tool.into(), full_output_path };
    McpToolOutput { content, details, structured_content, is_error: result.is_error() }
}
