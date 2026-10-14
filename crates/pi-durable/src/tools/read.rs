//! The `read` tool: text files with offsets and limits, and images. Remarks about truncation, continuation, and image
//! processing are diagnostics; the content is only file text, or one image block.

use std::sync::Arc;

use async_trait::async_trait;
use pi_ai::{ContentBlock, JsonObject};
use pi_chord::Context;
use serde::Deserialize;
use serde_json::{Value, json};

use super::image::{ByteSource, detect_supported_image_mime_type_of, image_dimensions};
use super::path_utils::resolve_read_tool_path;
use super::{ImageModel, ToolCallApi, parse_args, require_env, thrown};
use crate::env::{BinaryReader, FileInfo, LineRange, LineScan, StreamDecoder, starts_with_bom};
use crate::errors::{DurableError, DurableResult};
use crate::harness::output::character_end;
use crate::harness::{DiagnosticSeverity, ToolDiagnostic, ToolExecutionResult, ToolRegistration, define_tool};
use crate::images::{INLINE_IMAGE_TYPES, ImageProcessor, base64_length, to_base64};
use crate::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TextTotals, format_size, truncate_head_of};

const READ_CHUNK: u64 = 64 * 1024;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

#[derive(Clone, Default)]
pub struct ReadToolOptions {
    /// Resizes and converts images; without one, images within the byte limit pass through as they are.
    pub images: Option<Arc<dyn ImageProcessor>>,
}

#[derive(Deserialize)]
struct ReadToolInput {
    path: String,
    offset: Option<f64>,
    limit: Option<f64>,
}

/// Reads text files and images.
pub fn create_read_tool(options: ReadToolOptions) -> ToolRegistration {
    let formats = if options.images.is_none() { "jpg, png, gif, webp" } else { "jpg, png, gif, webp, bmp" };
    let description = format!(
        "Read the contents of a file. Supports text files and images ({formats}). Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
        DEFAULT_MAX_BYTES / 1024
    );
    let parameters = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
            "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
            "limit": { "type": "number", "description": "Maximum number of lines to read" }
        },
        "required": ["path"]
    });
    let options = Arc::new(options);
    define_tool("read", description, parameters, move |args, api, context| {
        let options = options.clone();
        async move { execute_read(&options, args, Arc::new(api), context).await }
    })
}

/// The `read` tool's execution against any call api.
pub async fn execute_read(
    options: &ReadToolOptions,
    args: JsonObject,
    api: Arc<dyn ToolCallApi>,
    context: Context,
) -> DurableResult<ToolExecutionResult> {
    let ReadToolInput { path, offset, limit } = parse_args(args)?;
    let env = require_env(api.as_ref())?;
    let absolute = resolve_read_tool_path(env.as_ref(), &path, &context).await?;
    let reader = env.open_binary_reader(&absolute, false, &context).await.map_err(thrown)?;
    let result = read_opened(reader.as_ref(), options, &path, offset, limit, api.as_ref(), &context).await;
    reader.close(&context).await;
    result
}

/// A concurrent writer can change the file between the scan and the reads. Appending (a growing log) leaves the
/// scanned text as it was; an image must be whole. Otherwise the file is read again once.
async fn read_opened(
    reader: &dyn BinaryReader,
    options: &ReadToolOptions,
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
    api: &dyn ToolCallApi,
    context: &Context,
) -> DurableResult<ToolExecutionResult> {
    for attempt in 0.. {
        let before = reader.info(context).await.map_err(thrown)?;
        let source = ReaderSource { reader, size: before.size, context };
        let mime_type = detect_supported_image_mime_type_of(&source).await?;
        let result = match mime_type {
            None => read_text(reader, path, offset, limit, context).await?,
            Some(mime) => {
                let model = api.image_model(context).await?;
                read_image(reader, &before, path, mime, options.images.as_deref(), model, context).await?
            }
        };
        let after = reader.info(context).await.map_err(thrown)?;
        let unchanged = after.size == before.size && after.mtime_ms == before.mtime_ms;
        if unchanged || (mime_type.is_none() && after.size > before.size) {
            return Ok(result);
        }
        if attempt == 1 {
            break;
        }
    }
    Err(DurableError::error(format!("{path} changed while it was read")))
}

struct ReaderSource<'a> {
    reader: &'a dyn BinaryReader,
    size: u64,
    context: &'a Context,
}

#[async_trait]
impl ByteSource for ReaderSource<'_> {
    fn size(&self) -> u64 {
        self.size
    }

    async fn read(&self, offset: u64, length: u64) -> DurableResult<Vec<u8>> {
        self.reader.read(offset, length, self.context).await.map_err(thrown)
    }
}

fn image_error(message: String) -> ToolExecutionResult {
    let diagnostic = ToolDiagnostic::new(DiagnosticSeverity::Error, message).with_code("unsupported_image");
    ToolExecutionResult {
        output: Some(Vec::new()),
        is_error: Some(true),
        diagnostics: Some(vec![diagnostic]),
        ..Default::default()
    }
}

/// One image block, prepared by `processor` or, without one, as it is when its format is inline and it fits the
/// limits (its size checked before reading it, its dimensions from its header); otherwise an error result.
async fn read_image(
    reader: &dyn BinaryReader,
    info: &FileInfo,
    path: &str,
    mime_type: &str,
    processor: Option<&dyn ImageProcessor>,
    ImageModel { vision, limits }: ImageModel,
    context: &Context,
) -> DurableResult<ToolExecutionResult> {
    let mut notes = Vec::new();
    let (data, image_type) = if let Some(processor) = processor {
        let bytes = reader.read(0, info.size, context).await.map_err(thrown)?;
        let Some(prepared) = processor.prepare(&bytes, mime_type, limits).await else {
            return Ok(image_error(format!("{path} is an image ({mime_type}) that cannot be prepared for the model")));
        };
        if let Some(from) = &prepared.converted_from {
            notes.push(format!("Converted from {from} to {}.", prepared.mime_type));
        }
        if let Some(resize) = prepared.resized {
            let (from, to) = (resize.from, resize.to);
            let scale = from.width as f64 / to.width as f64;
            notes.push(format!(
                "Resized from {}x{} to {}x{}. Multiply coordinates by {scale:.2} to map them to the original.",
                from.width, from.height, to.width, to.height
            ));
        }
        (prepared.data, prepared.mime_type)
    } else {
        if !INLINE_IMAGE_TYPES.contains(&mime_type) {
            let message =
                format!("{path} is an image ({mime_type}) that needs converting, and no image processor is configured");
            return Ok(image_error(message));
        }
        let unshrinkable = "and no image processor is configured to shrink it";
        if base64_length(info.size as usize) > limits.max_bytes {
            let size = format_size(info.size);
            return Ok(image_error(format!(
                "{path} is an image ({mime_type}) of {size}, too large to send, {unshrinkable}"
            )));
        }
        let bytes = reader.read(0, info.size, context).await.map_err(thrown)?;
        let Some(size) = image_dimensions(&bytes, mime_type) else {
            return Ok(image_error(format!("{path} is an image ({mime_type}) whose size cannot be read")));
        };
        if size.width > limits.max_width || size.height > limits.max_height {
            return Ok(image_error(format!(
                "{path} is an image ({mime_type}) of {}x{}, larger than {}x{}, {unshrinkable}",
                size.width, size.height, limits.max_width, limits.max_height
            )));
        }
        (to_base64(&bytes), mime_type.to_string())
    };
    if !vision {
        notes.push("The current model does not support images; it sees a placeholder instead.".into());
    }
    let message =
        std::iter::once(format!("Read image file [{image_type}].")).chain(notes).collect::<Vec<_>>().join(" ");
    Ok(ToolExecutionResult {
        output: Some(vec![ContentBlock::Image { data, mime_type: image_type }]),
        diagnostics: Some(vec![ToolDiagnostic::new(DiagnosticSeverity::Info, message).with_code("image")]),
        ..Default::default()
    })
}

/// `Array.prototype.slice`'s conversion of an index: NaN is 0, other values truncate toward zero.
fn slice_index(value: f64) -> f64 {
    if value.is_nan() { 0.0 } else { value.trunc() }
}

fn is_safe_integer(value: f64) -> bool {
    value.is_finite() && value.trunc() == value && value.abs() <= MAX_SAFE_INTEGER
}

/// A number as JavaScript prints it in a template, for the integral and decimal values messages show.
fn js_number(value: f64) -> String {
    let value = value + 0.0;
    if value.trunc() == value && value.abs() < 1e21 { format!("{value:.0}") } else { format!("{value}") }
}

/// The decoded start of bytes `[start, end)` of the file, decoded as part of the whole file: all of it, or enough for
/// `truncate_head_of` (more than `DEFAULT_MAX_BYTES + 1` bytes, or `DEFAULT_MAX_LINES` newlines).
async fn read_head(
    reader: &dyn BinaryReader,
    start: u64,
    end: u64,
    skip_bom: bool,
    context: &Context,
) -> DurableResult<String> {
    let mut decoder = StreamDecoder::range();
    let (mut text, mut newlines) = (String::new(), 0usize);
    let mut position = if skip_bom && start == 0 { 3 } else { start };
    while position < end {
        let bytes = reader.read(position, READ_CHUNK.min(end - position), context).await.map_err(thrown)?;
        if bytes.is_empty() {
            break;
        }
        position += bytes.len() as u64;
        let decoded = decoder.decode(&bytes);
        newlines += decoded.matches('\n').count();
        text.push_str(&decoded);
        if newlines >= DEFAULT_MAX_LINES || text.len() > DEFAULT_MAX_BYTES + 1 {
            return Ok(text);
        }
    }
    text.push_str(&decoder.finish());
    Ok(text)
}

/// The result for the opened file: decoding the whole file, splitting it on `\n`, and bounding the selected lines
/// with `truncate_head` would give the same, but only one scan of the file plus the head is read.
async fn read_text(
    reader: &dyn BinaryReader,
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
    context: &Context,
) -> DurableResult<ToolExecutionResult> {
    let start_line = match offset {
        Some(offset) if offset != 0.0 && !offset.is_nan() => (offset - 1.0).max(0.0),
        _ => 0.0,
    };
    let start_display = start_line + 1.0;
    // Lines are selected like `allLines.slice(startLine, endLine)`; a start beyond any file is scanned from 0 only
    // to count lines, and an empty selection (a zero or negative limit) is scanned as one line and then ignored.
    let slice_start = slice_index(start_line);
    let scan_start = if is_safe_integer(slice_start) { slice_start } else { 0.0 };
    let requested_end = limit.map(|limit| (scan_start + 1.0).max(slice_index(start_line + limit)));
    let scan_end = requested_end.filter(|end| is_safe_integer(*end));
    let scan_of = |end_line: Option<f64>| async move {
        let range = LineRange { start_line: scan_start as u64, end_line: end_line.map(|end| end as u64) };
        reader.scan_lines(range, context).await.map_err(thrown)
    };
    let mut scan: LineScan = scan_of(scan_end).await?;
    let total_lines = (scan.newlines + 1) as f64;
    if start_line >= total_lines {
        let offset = offset.map_or_else(|| "undefined".into(), js_number);
        let total = js_number(total_lines);
        return Err(DurableError::error(format!("Offset {offset} is beyond end of file ({total} lines total)")));
    }

    let mut user_limited = None;
    let mut selected = total_lines - slice_start;
    if let Some(limit) = limit {
        let end_line = if (start_line + limit).is_nan() { f64::NAN } else { (start_line + limit).min(total_lines) };
        user_limited = Some(end_line - start_line);
        // `slice` counts a negative end from the end of the lines, which only the line count tells; scan again.
        let relative_end = slice_index(end_line);
        let slice_end = if relative_end < 0.0 { (total_lines + relative_end).max(0.0) } else { relative_end };
        selected = (slice_end - slice_start).max(0.0);
        if selected > 0.0 && relative_end < 0.0 {
            scan = scan_of(Some(slice_end)).await?;
        }
    }
    let empty = selected == 0.0;
    // Counted like `truncate_head`: a trailing newline adds no line, and empty text has none.
    let ends_with_newline = !empty && scan.last_line_start == scan.end && scan.last_line_start > scan.start;
    let lines = if empty || scan.selected_bytes == 0 { 0.0 } else { selected - f64::from(u8::from(ends_with_newline)) };
    let totals = TextTotals { lines: lines as usize, bytes: if empty { 0 } else { scan.selected_bytes as usize } };
    let first_bytes = reader.read(0, 3, context).await.map_err(thrown)?;
    let head = if empty {
        String::new()
    } else {
        read_head(reader, scan.start, scan.end, starts_with_bom(&first_bytes), context).await?
    };

    let truncation = truncate_head_of(&head, totals, Default::default());
    let mut diagnostics = Vec::new();
    let mut output = truncation.content.clone();
    let mut details = None;
    let (sld, max) = (js_number(start_display), format_size(DEFAULT_MAX_BYTES as u64));
    if truncation.first_line_exceeds_limit {
        // Show the start of the line, cut at the byte limit on a character boundary. Like `allLines[startLine]`, a
        // fractional start line names no line.
        let integral = start_line.trunc() == start_line;
        let line = if integral { head.split('\n').next().unwrap_or("") } else { "" };
        let line_size = if integral { scan.first_line_bytes } else { 0 };
        let end = character_end(line.as_bytes(), DEFAULT_MAX_BYTES).min(line.len());
        output = line[..end].to_string();
        let message = format!(
            "Line {sld} is {}, exceeds the {max} limit; showing its first {}. Use bash: sed -n '{sld}p' {path} | tail -c +{}",
            format_size(line_size),
            format_size(end as u64),
            end + 1
        );
        diagnostics.push(ToolDiagnostic::new(DiagnosticSeverity::Warn, message).with_code("truncated"));
        let mut value = truncation_details(&truncation)?;
        value["outputBytes"] = json!(end);
        value["outputLines"] = json!(1);
        details = Some(json!({ "truncation": value }));
    } else if truncation.truncated {
        let end_display = start_display + truncation.output_lines as f64 - 1.0;
        let by_lines = truncation.truncated_by == Some(crate::truncate::TruncatedBy::Lines);
        let limit_text = if by_lines { String::new() } else { format!(" ({max} limit)") };
        let message = format!(
            "Showing lines {sld}-{} of {}{limit_text}. Use offset={} to continue.",
            js_number(end_display),
            js_number(total_lines),
            js_number(end_display + 1.0)
        );
        diagnostics.push(ToolDiagnostic::new(DiagnosticSeverity::Info, message).with_code("truncated"));
        details = Some(json!({ "truncation": truncation_details(&truncation)? }));
    } else if let Some(limited) = user_limited.filter(|limited| start_line + limited < total_lines) {
        let remaining = total_lines - (start_line + limited);
        let next = start_line + limited + 1.0;
        let message =
            format!("{} more lines in file. Use offset={} to continue.", js_number(remaining), js_number(next));
        diagnostics.push(ToolDiagnostic::new(DiagnosticSeverity::Info, message));
    }
    Ok(ToolExecutionResult {
        output: Some(if output.is_empty() { Vec::new() } else { vec![ContentBlock::text(output)] }),
        details: details.map(Into::into),
        diagnostics: Some(diagnostics),
        ..Default::default()
    })
}

/// The truncation without its content: the shown text is the result content.
fn truncation_details(truncation: &crate::truncate::TruncationResult) -> DurableResult<Value> {
    let mut value = serde_json::to_value(truncation)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("content");
    }
    Ok(value)
}
