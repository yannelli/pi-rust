//! `read` tool: text files with offset/limit and head truncation, images as attachments.

use std::path::PathBuf;

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, ToolContext, ToolUpdateCallback};
use pi_ai::{ContentBlock, ImageResizeOptions, Model};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::path_utils::resolve_read_path;
use super::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, format_size, truncate_head,
};
use super::{as_count, check_aborted, parse_args};
use crate::images::{ProcessImageOptions, process_image};

const IMAGE_TYPE_SNIFF_BYTES: usize = 4100;
const NON_VISION_IMAGE_NOTE: &str =
    "[Current model does not support images. The image will be omitted from this request.]";
const PNG_SIGNATURE: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

pub struct ReadTool {
    cwd: PathBuf,
    description: String,
    auto_resize: bool,
    /// Resize profile for calls without a model.
    resize: Option<ImageResizeOptions>,
    /// Note that images are dropped, for calls without a model.
    non_vision_note: bool,
}

/// Options of the read tool (upstream `ReadToolOptions`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReadToolOptions {
    /// Resize images to the model's limits (`images.autoResize`). Default: true.
    pub auto_resize_images: bool,
    /// Resize profile when the call's model has none.
    pub resize_options: Option<ImageResizeOptions>,
}

impl Default for ReadToolOptions {
    fn default() -> Self {
        Self { auto_resize_images: true, resize_options: None }
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    offset: Option<f64>,
    limit: Option<f64>,
}

impl ReadTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_options(cwd, ReadToolOptions::default())
    }

    /// Calls inside the agent loop use their turn's model for the resize profile and the
    /// note about models without image input; `options` cover calls without one.
    pub fn with_options(cwd: PathBuf, options: ReadToolOptions) -> Self {
        let description = format!(
            "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
            DEFAULT_MAX_BYTES / 1024
        );
        Self {
            cwd,
            description,
            auto_resize: options.auto_resize_images,
            resize: options.resize_options,
            non_vision_note: false,
        }
    }

    /// Images are processed with `options`, for example [`ProcessImageOptions::for_model`].
    pub fn with_image_options(cwd: PathBuf, image_options: ProcessImageOptions) -> Self {
        let options =
            ReadToolOptions { auto_resize_images: image_options.auto_resize, resize_options: image_options.resize };
        Self::with_options(cwd, options)
    }

    /// Resize profile from the model's `inputLimits`, plus the note for models without vision.
    pub fn for_model(cwd: PathBuf, model: &pi_ai::Model, auto_resize: bool) -> Self {
        let mut tool = Self::with_image_options(cwd, ProcessImageOptions::for_model(model, auto_resize));
        tool.non_vision_note = !supports_images(model);
        tool
    }

    async fn read(
        &self,
        args: Value,
        model: Option<&Model>,
        cancel: CancellationToken,
    ) -> anyhow::Result<AgentToolResult> {
        let args: ReadArgs = parse_args("read", args)?;
        check_aborted(&cancel)?;
        let absolute = resolve_read_path(&args.path, &self.cwd);
        let buffer =
            tokio::fs::read(&absolute).await.map_err(|e| anyhow!("Could not read file {}: {e}", absolute.display()))?;
        check_aborted(&cancel)?;

        let sniff = &buffer[..buffer.len().min(IMAGE_TYPE_SNIFF_BYTES)];
        let Some(mime_type) = detect_supported_image_mime_type(sniff) else {
            return read_text(&String::from_utf8_lossy(&buffer), &args);
        };
        let resize = model.and_then(|model| ProcessImageOptions::for_model(model, self.auto_resize).resize);
        let options = ProcessImageOptions { auto_resize: self.auto_resize, resize: resize.or(self.resize) };
        let processed = tokio::task::spawn_blocking(move || process_image(&buffer, mime_type, &options)).await?;
        check_aborted(&cancel)?;
        let non_vision = model.map_or(self.non_vision_note, |model| !supports_images(model));
        let vision_note = if non_vision { format!("\n{NON_VISION_IMAGE_NOTE}") } else { String::new() };
        let content = match processed {
            Ok(image) => {
                let mut note = format!("Read image file [{}]", image.mime_type);
                if !image.hints.is_empty() {
                    note.push('\n');
                    note.push_str(&image.hints.join("\n"));
                }
                note.push_str(&vision_note);
                vec![ContentBlock::text(note), ContentBlock::Image { data: image.data, mime_type: image.mime_type }]
            }
            Err(message) => vec![ContentBlock::text(format!("Read image file [{mime_type}]\n{message}{vision_note}"))],
        };
        Ok(AgentToolResult { content, ..Default::default() })
    }
}

fn supports_images(model: &Model) -> bool {
    model.input.iter().any(|input| input == "image")
}

/// The image and its note, or the text for text files and images that could not be processed.
fn to_read_output(content: &[ContentBlock]) -> Value {
    let text = content.iter().find_map(ContentBlock::as_text).unwrap_or_default();
    let image = content.iter().find_map(|block| match block {
        ContentBlock::Image { data, mime_type } => Some((data, mime_type)),
        _ => None,
    });
    match image {
        Some((data, mime_type)) => json!({ "type": "image", "data": data, "mimeType": mime_type, "note": text }),
        None => json!(text),
    }
}

/// Schema of the read tool's `structured_content`.
pub fn read_output_schema() -> Value {
    json!({
        "anyOf": [
            { "type": "string" },
            {
                "type": "object",
                "properties": {
                    "type": { "const": "image", "type": "string" },
                    "data": { "type": "string" },
                    "mimeType": { "type": "string" },
                    "note": { "type": "string" }
                },
                "required": ["type", "data", "mimeType", "note"]
            }
        ]
    })
}

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
                "limit": { "type": "number", "description": "Maximum number of lines to read" }
            },
            "required": ["path"]
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(read_output_schema())
    }

    fn constrained_sampling(&self) -> Option<Value> {
        Some(super::prefer_json_schema())
    }

    fn prompt_snippet(&self) -> Option<&str> {
        super::builtin_snippet("read")
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        super::builtin_guidelines("read")
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let result = self.read(args, None, cancel).await?;
        let structured = to_read_output(&result.content);
        Ok(result.with_structured_content(structured))
    }

    async fn execute_with_context(
        &self,
        context: &ToolContext,
        args: Value,
        cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let result = self.read(args, context.model(), cancel).await?;
        let structured = to_read_output(&result.content);
        Ok(result.with_structured_content(structured))
    }
}

fn read_text(text: &str, args: &ReadArgs) -> anyhow::Result<AgentToolResult> {
    let all_lines: Vec<&str> = text.split('\n').collect();
    let total = all_lines.len();
    let offset = as_count(args.offset);
    let start = offset.filter(|o| *o != 0).map_or(0, |o| (o - 1).max(0) as usize);
    let start_display = start + 1;
    if start >= total {
        let shown = args.offset.map(|o| o.to_string()).unwrap_or_default();
        bail!("Offset {shown} is beyond end of file ({total} lines total)");
    }

    let mut user_limited = None;
    let selected = match as_count(args.limit) {
        Some(limit) => {
            let end = (start + limit.max(0) as usize).min(total);
            user_limited = Some(end.saturating_sub(start));
            all_lines[start..end.max(start)].join("\n")
        }
        None => all_lines[start..].join("\n"),
    };

    let truncation = truncate_head(&selected, TruncationOptions::default());
    let mut details = None;
    let output = if truncation.first_line_exceeds_limit {
        let size = format_size(all_lines[start].len());
        let limit = format_size(DEFAULT_MAX_BYTES);
        let path = &args.path;
        details = Some(json!({ "truncation": truncation }));
        format!(
            "[Line {start_display} is {size}, exceeds {limit} limit. Use bash: sed -n '{start_display}p' {path} | head -c {DEFAULT_MAX_BYTES}]"
        )
    } else if truncation.truncated {
        let end_display = start_display + truncation.output_lines - 1;
        let next = end_display + 1;
        let notice = if truncation.truncated_by == Some(TruncatedBy::Lines) {
            format!("[Showing lines {start_display}-{end_display} of {total}. Use offset={next} to continue.]")
        } else {
            let limit = format_size(DEFAULT_MAX_BYTES);
            format!(
                "[Showing lines {start_display}-{end_display} of {total} ({limit} limit). Use offset={next} to continue.]"
            )
        };
        let out = format!("{}\n\n{notice}", truncation.content);
        details = Some(json!({ "truncation": truncation }));
        out
    } else if let Some(count) = user_limited.filter(|n| start + n < total) {
        let remaining = total - (start + count);
        let next = start + count + 1;
        format!("{}\n\n[{remaining} more lines in file. Use offset={next} to continue.]", truncation.content)
    } else {
        truncation.content
    };

    Ok(AgentToolResult { content: vec![ContentBlock::text(output)], details, ..Default::default() })
}

fn read_u32_be(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn read_u32_le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn read_u16_le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn ascii_at(b: &[u8], at: usize, s: &str) -> bool {
    b.get(at..at + s.len()) == Some(s.as_bytes())
}

/// Detect png/jpeg/gif/webp/bmp from magic bytes, as upstream `detectSupportedImageMimeType`.
pub(crate) fn detect_supported_image_mime_type(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0xff, 0xd8, 0xff]) {
        return (b.get(3) != Some(&0xf7)).then_some("image/jpeg");
    }
    if b.starts_with(PNG_SIGNATURE) {
        return (is_png(b) && !is_animated_png(b)).then_some("image/png");
    }
    if ascii_at(b, 0, "GIF87a") || ascii_at(b, 0, "GIF89a") {
        return Some("image/gif");
    }
    if ascii_at(b, 0, "RIFF") && ascii_at(b, 8, "WEBP") {
        return Some("image/webp");
    }
    if ascii_at(b, 0, "BM") && is_bmp(b) {
        return Some("image/bmp");
    }
    None
}

fn is_png(b: &[u8]) -> bool {
    b.len() >= 16 && read_u32_be(b, PNG_SIGNATURE.len()) == 13 && ascii_at(b, 12, "IHDR")
}

fn is_animated_png(b: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= b.len() {
        let chunk_len = read_u32_be(b, offset) as usize;
        if ascii_at(b, offset + 4, "acTL") {
            return true;
        }
        if ascii_at(b, offset + 4, "IDAT") {
            return false;
        }
        let next = offset.saturating_add(12).saturating_add(chunk_len);
        if next <= offset || next > b.len() {
            return false;
        }
        offset = next;
    }
    false
}

fn is_bmp(b: &[u8]) -> bool {
    if b.len() < 26 {
        return false;
    }
    let file_size = read_u32_le(b, 2);
    let pixel_offset = read_u32_le(b, 10);
    let dib_size = read_u32_le(b, 14);
    if file_size != 0 && file_size < 26 {
        return false;
    }
    if pixel_offset < 14u32.saturating_add(dib_size) || (file_size != 0 && pixel_offset >= file_size) {
        return false;
    }
    let (planes, bpp) = if dib_size == 12 {
        (read_u16_le(b, 22), read_u16_le(b, 24))
    } else if (40..=124).contains(&dib_size) {
        if b.len() < 30 {
            return false;
        }
        (read_u16_le(b, 26), read_u16_le(b, 28))
    } else {
        return false;
    };
    planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bpp)
}
