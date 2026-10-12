//! Lays out a script's output for the model: text item markers, the console block, the token
//! budget with a temp file for the full text, and temp files for images.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use base64::Engine;
use pi_ai::ContentBlock;
use serde_json::Value;

use crate::js::{js_stringify, js_to_fixed, utf16_len, utf16_slice, utf16_tail};
use crate::sandbox::{CodemodeError, CodemodeErrorKind, CodemodeOutputItem};

/// Default token budget for script output.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 10_000;
/// Characters per token when estimating.
const CHARS_PER_TOKEN: usize = 4;
const OUTPUT_FILE_PREFIX: &str = "pi-codemode";

/// Like the script's `text()`: strings as is, other values as compact JSON.
pub fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => js_stringify(other),
    }
}

fn text_block(text: impl Into<String>) -> ContentBlock {
    ContentBlock::text(text)
}

/// With more than one text item, each starts with a `==> text N/M <==` line. `console.*` lines
/// follow all other output in one `<console_output>` block.
pub fn format_output(output: &[CodemodeOutputItem]) -> Vec<ContentBlock> {
    let total = output.iter().filter(|item| matches!(item, CodemodeOutputItem::Text { console: false, .. })).count();
    let mut items = Vec::new();
    let mut console_lines = Vec::new();
    let mut index = 0;
    for item in output {
        match item {
            CodemodeOutputItem::Image { data, mime_type } => {
                items.push(ContentBlock::Image { data: data.clone(), mime_type: mime_type.clone() })
            }
            CodemodeOutputItem::Text { text, console: true } => console_lines.push(text.as_str()),
            CodemodeOutputItem::Text { text, console: false } => {
                index += 1;
                items.push(text_block(if total > 1 {
                    format!("==> text {index}/{total} <==\n{text}")
                } else {
                    text.clone()
                }));
            }
        }
    }
    if !console_lines.is_empty() {
        items.push(text_block(format!("<console_output>\n{}\n</console_output>", console_lines.join("\n"))));
    }
    items
}

/// Merge each run of text items into one, each part starting on its own line.
pub fn join_text_runs(items: Vec<ContentBlock>) -> Vec<ContentBlock> {
    let mut joined: Vec<ContentBlock> = Vec::new();
    for item in items {
        if let (ContentBlock::Text { text, .. }, Some(ContentBlock::Text { text: last, .. })) =
            (&item, joined.last_mut())
        {
            if !last.is_empty() && !last.ends_with('\n') {
                last.push('\n');
            }
            last.push_str(text);
            continue;
        }
        joined.push(item);
    }
    joined
}

/// Text of a failed script: the stack or error, and the tool calls that already ran.
pub fn format_error(error: &CodemodeError, call_summary: &str) -> String {
    let head = match error.kind {
        CodemodeErrorKind::Script => error
            .stack
            .clone()
            .unwrap_or_else(|| format!("{}: {}", error.name.as_deref().unwrap_or("Error"), error.message)),
        CodemodeErrorKind::Timeout => format!("Script timed out: {}", error.message),
        CodemodeErrorKind::Aborted => format!("Script aborted: {}", error.message),
        CodemodeErrorKind::Sandbox => format!("Script sandbox failed: {}", error.message),
    };
    format!("{head}\n\n{call_summary}")
}

/// `formatSize` of the coding agent: bytes, KB, or MB with one decimal.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{}KB", js_to_fixed(bytes as f64 / 1024.0, 1))
    } else {
        format!("{}MB", js_to_fixed(bytes as f64 / (1024.0 * 1024.0), 1))
    }
}

/// Write `data` to a new `<tmp>/pi-codemode-<random hex><extension>` readable only by the user.
pub fn write_output_file(extension: &str, data: &[u8]) -> std::io::Result<PathBuf> {
    use std::hash::{BuildHasher, Hasher};
    let mut last_error = None;
    for attempt in 0..8u32 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos());
        hasher.write_u128(nanos.unwrap_or_default());
        hasher.write_u32(std::process::id() ^ attempt);
        let path = std::env::temp_dir().join(format!("{OUTPUT_FILE_PREFIX}-{:016x}{extension}", hasher.finish()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(data)?;
                return Ok(path);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| std::io::Error::other("could not create an output file")))
}

/// File extensions of the image types `image()` accepts.
fn image_extension(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "image/png" => Some(".png"),
        "image/jpeg" => Some(".jpg"),
        "image/gif" => Some(".gif"),
        "image/webp" => Some(".webp"),
        _ => None,
    }
}

/// Decodes like Node's `Buffer.from(data, "base64")`: padding optional, trailing bits ignored.
fn lenient_base64() -> base64::engine::GeneralPurpose {
    let config = base64::engine::GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent);
    base64::engine::GeneralPurpose::new(&base64::alphabet::STANDARD, config)
}

fn image_label(data: &str, mime_type: &str) -> String {
    let bytes = lenient_base64().decode(data).unwrap_or_default();
    let kind = format!("{mime_type}, {}", format_size(bytes.len()));
    let saved = match image_extension(mime_type) {
        Some(extension) => write_output_file(extension, &bytes).map_err(|error| error.to_string()),
        None => Err(format!("No file extension for image type {mime_type}")),
    };
    // A failed write must not discard the result of a script whose tool calls already ran.
    match saved {
        Ok(path) => format!("[Image saved to {} ({kind})]", path.display()),
        Err(error) => format!("[Image ({kind}) could not be saved: {error}]"),
    }
}

/// Save each image to a temp file and put a text item with its path before it. Images shown more
/// than once are saved once.
pub fn save_images(items: Vec<ContentBlock>) -> Vec<ContentBlock> {
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        if let ContentBlock::Image { data, mime_type } = &item {
            let label = labels.entry(data.clone()).or_insert_with(|| image_label(data, mime_type)).clone();
            result.push(text_block(label));
        }
        result.push(item);
    }
    result
}

/// Output within the token budget, and the temp file with the full text when it was truncated.
pub struct TruncatedOutput {
    pub items: Vec<ContentBlock>,
    pub full_output_path: Option<String>,
}

/// When the combined text exceeds the budget, the text items become one item that keeps the start
/// and end of the text, followed by the images. The full text goes to a temp file.
pub fn truncate_output(items: Vec<ContentBlock>, max_tokens: u64) -> TruncatedOutput {
    let texts: Vec<&str> = items.iter().filter_map(ContentBlock::as_text).collect();
    let combined = texts.join("\n");
    let length = utf16_len(&combined);
    let budget = usize::try_from(max_tokens).unwrap_or(usize::MAX).saturating_mul(CHARS_PER_TOKEN);
    if texts.is_empty() || length <= budget {
        return TruncatedOutput { items, full_output_path: None };
    }
    let head_chars = budget / 2;
    let tail_chars = budget - head_chars;
    let removed = length - head_chars - tail_chars;
    let head = utf16_slice(&combined, 0, head_chars);
    let tail = if tail_chars > 0 { utf16_tail(&combined, tail_chars) } else { "" };
    let mut text = format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {}\n\n{head}…{} tokens truncated…{tail}",
        length.div_ceil(CHARS_PER_TOKEN),
        combined.split('\n').count(),
        removed.div_ceil(CHARS_PER_TOKEN),
    );
    let spilled = write_output_file(".txt", combined.as_bytes());
    match &spilled {
        Ok(path) => text.push_str(&format!("\n\n[Full output: {} (read with offset/limit)]", path.display())),
        Err(error) => text.push_str(&format!("\n\n[Could not save the full output: {error}]")),
    }
    let mut truncated = vec![text_block(text)];
    truncated.extend(items.into_iter().filter(|item| matches!(item, ContentBlock::Image { .. })));
    TruncatedOutput { items: truncated, full_output_path: spilled.ok().map(|path| path.display().to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes_like_upstream() {
        assert_eq!(format_size(70), "70B");
        assert_eq!(format_size(1280), "1.3KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0MB");
    }

    #[test]
    fn merges_text_runs_on_new_lines() {
        let image = ContentBlock::Image { data: "x".into(), mime_type: "image/png".into() };
        let joined =
            join_text_runs(vec![text_block("a"), text_block("b\n"), text_block("c"), image.clone(), text_block("")]);
        assert_eq!(joined, vec![text_block("a\nb\nc"), image, text_block("")]);
    }
}
