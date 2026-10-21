//! A script's result as the model reads it, as in the coding agent's code mode: a header with the wall time, the text
//! items, the `console` block, the returned value, and on failure the error with the calls already made. Text past the
//! budget is cut to its start and end, with the full text in a file; images are saved to files too.

use std::collections::HashMap;

use base64::Engine;
use futures::future::BoxFuture;
use pi_ai::ContentBlock;
use pi_codemode::js::{js_to_fixed, utf16_len, utf16_slice, utf16_tail};
use pi_codemode::output::{format_error, format_output, join_text_runs, value_text};
use pi_codemode::{CodemodeOutputItem, CodemodeResult};

use super::details::CodemodeNestedCall;

/// Writes a file of the result under a fixed name: its path, or why it could not be written.
pub(super) trait OutputFile: Send + Sync {
    fn write<'a>(&'a self, name: &'a str, data: Vec<u8>) -> BoxFuture<'a, Result<String, String>>;
}

/// The tool's output limits, which the Harness enforces by cutting without a spill file: results stay inside them.
#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub max_bytes: usize,
    pub max_lines: usize,
}

/// Lines of the result besides the script's text: the header, markers, notes.
const LINE_MARGIN: usize = 100;
/// Characters per token when estimating.
const CHARS_PER_TOKEN: usize = 4;

/// File extension of an image type `image()` accepts.
fn image_extension(mime_type: &str) -> &'static str {
    match mime_type {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        _ => ".bin",
    }
}

/// Ceiling of `value / 4`, as `Math.ceil` rounds a negative count.
fn tokens(value: i64) -> i64 {
    (value as f64 / CHARS_PER_TOKEN as f64).ceil() as i64
}

/// Output within the budget, and the file with the complete text when it was cut.
struct Truncated {
    items: Vec<ContentBlock>,
    full_output_path: Option<String>,
}

/// Apply the token budget: when the combined text exceeds it, the text items become one item that keeps the start and
/// end of the text, and images follow it. The complete text is written to a file.
async fn truncate(items: Vec<ContentBlock>, max_tokens: usize, limits: Limits, write: &dyn OutputFile) -> Truncated {
    let texts: Vec<&str> = items.iter().filter_map(ContentBlock::as_text).collect();
    let combined = texts.join("\n");
    let length = utf16_len(&combined);
    // Within the limits too: a UTF-16 unit is at most 3 UTF-8 bytes, and the rest of the result is a few short lines.
    let budget = max_tokens.saturating_mul(CHARS_PER_TOKEN).min(limits.max_bytes / 4);
    let line_budget = limits.max_lines.saturating_sub(LINE_MARGIN) / 2;
    let lines = combined.split('\n').count();
    if texts.is_empty() || (length <= budget && lines <= 2 * line_budget) {
        return Truncated { items, full_output_path: None };
    }
    let head_chars = budget / 2;
    let tail_chars = budget - head_chars;
    let head = utf16_slice(&combined, 0, head_chars).split('\n').take(line_budget).collect::<Vec<_>>().join("\n");
    let tail = if tail_chars > 0 {
        let tail: Vec<&str> = utf16_tail(&combined, tail_chars).split('\n').collect();
        tail[tail.len().saturating_sub(line_budget)..].join("\n")
    } else {
        String::new()
    };
    let removed = length as i64 - utf16_len(&head) as i64 - utf16_len(&tail) as i64;
    let mut text = format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {lines}\n\n{head}…{} tokens truncated…{tail}",
        tokens(length as i64),
        tokens(removed),
    );
    let spilled = write.write("output.txt", combined.clone().into_bytes()).await;
    match &spilled {
        Ok(path) => text.push_str(&format!("\n\n[Full output: {path} (read with offset/limit)]")),
        Err(error) => text.push_str(&format!("\n\n[Could not save the full output: {error}]")),
    }
    let mut truncated = vec![ContentBlock::text(text)];
    truncated.extend(items.into_iter().filter(|item| matches!(item, ContentBlock::Image { .. })));
    Truncated { items: truncated, full_output_path: spilled.ok() }
}

/// Decodes like `atob` would for valid data, and never fails: padding optional, trailing bits ignored.
fn decode_base64(data: &str) -> Vec<u8> {
    let config = base64::engine::GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent);
    base64::engine::GeneralPurpose::new(&base64::alphabet::STANDARD, config).decode(data).unwrap_or_default()
}

/// Save each image to a file and put a text item with its path before it; an image shown twice is saved once.
async fn save_images(items: Vec<ContentBlock>, write: &dyn OutputFile) -> Vec<ContentBlock> {
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        if let ContentBlock::Image { data, mime_type } = &item {
            if !labels.contains_key(data) {
                let bytes = decode_base64(data);
                let kind = format!("{mime_type}, {} bytes", bytes.len());
                let name = format!("image-{}{}", labels.len() + 1, image_extension(mime_type));
                // A failed write must not discard the result of a script whose calls already ran.
                let label = match write.write(&name, bytes).await {
                    Ok(path) => format!("[Image saved to {path} ({kind})]"),
                    Err(error) => format!("[Image ({kind}) could not be saved: {error}]"),
                };
                labels.insert(data.clone(), label);
            }
            result.push(ContentBlock::text(labels[data].clone()));
        }
        result.push(item);
    }
    result
}

/// What `format_result` needs besides the script's result.
pub(super) struct FormatOptions<'a> {
    /// Images `models.generateImages()` returned; when the script showed none, the result says so.
    pub generated_images: usize,
    pub wall_ms: f64,
    pub max_output_tokens: usize,
    pub calls: &'a [CodemodeNestedCall],
    pub limits: Limits,
    pub write: &'a dyn OutputFile,
}

/// The calls a failed script made, which its error lists.
fn calls_summary(calls: &[CodemodeNestedCall]) -> String {
    if calls.is_empty() {
        return "No tool calls were made.".to_owned();
    }
    let status = |call: &CodemodeNestedCall| serde_json::to_value(call.status).ok();
    let listed: Vec<String> = calls
        .iter()
        .map(|call| format!("{} ({})", call.name, status(call).as_ref().and_then(|s| s.as_str()).unwrap_or_default()))
        .collect();
    format!("Tool calls made before the failure (they are not undone): {}", listed.join(", "))
}

/// The tool result's content for a script's result, and the path of the complete text when it was cut.
pub(super) async fn format_result(
    result: &CodemodeResult,
    options: FormatOptions<'_>,
) -> (Vec<ContentBlock>, Option<String>) {
    let mut output = result.output.clone();
    if let Ok(success) = &result.outcome
        && let Some(value) = &success.value
    {
        output.push(CodemodeOutputItem::Text { text: value_text(value), console: false });
    }
    let mut items = format_output(&output);
    if let Err(error) = &result.outcome {
        let text = format_error(error, &calls_summary(options.calls));
        items.push(ContentBlock::text(format!("Script error:\n{text}")));
    }
    let generated = options.generated_images;
    if generated > 0 && !items.iter().any(|item| matches!(item, ContentBlock::Image { .. })) {
        let plural = if generated == 1 { "" } else { "s" };
        items.push(ContentBlock::text(format!(
            "Note: models.generateImages() returned {generated} image{plural} that the script did not show. Show each image block of result.output with image(block)."
        )));
    }
    let truncated = truncate(join_text_runs(items), options.max_output_tokens, options.limits, options.write).await;
    // After truncation, which joins the text items and moves images after them, so each path stays next to its image.
    let content = join_text_runs(save_images(truncated.items, options.write).await);
    let header = format!(
        "{}\nWall time {} seconds\nOutput:\n",
        if result.is_ok() { "Script completed" } else { "Script failed" },
        js_to_fixed(options.wall_ms / 1000.0, 1)
    );
    let mut blocks = vec![ContentBlock::text(header)];
    blocks.extend(content);
    (blocks, truncated.full_output_path)
}
