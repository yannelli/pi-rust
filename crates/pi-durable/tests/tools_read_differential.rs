//! Upstream `test/tools-read-differential.test.ts`: `read` must give what reading the whole file gave before bounded
//! reads, while it reads only a bounded part of the file.

#[path = "support/tools_support.rs"]
mod support;

use pi_ai::ContentBlock;
use pi_durable::env::{LocalEnvOptions, LocalExecutionEnv};
use pi_durable::harness::output::character_end;
use pi_durable::harness::{DiagnosticSeverity, ToolDiagnostic, ToolExecutionResult};
use pi_durable::images::to_base64;
use pi_durable::tools::image::detect_supported_image_mime_type;
use pi_durable::tools::{ReadToolOptions, execute_read};
use pi_durable::truncate::{DEFAULT_MAX_BYTES, TruncatedBy, format_size, truncate_head};
use serde_json::json;
use std::sync::Arc;
use support::*;

/// A number as JavaScript prints it in a template.
fn js(value: f64) -> String {
    let value = value + 0.0;
    if value.trunc() == value && value.abs() < 1e21 { format!("{value:.0}") } else { format!("{value}") }
}

/// `Array.prototype.slice(start, end)` of `lines`, with JavaScript's index conversion.
fn slice<'a>(lines: &[&'a str], start: f64, end: Option<f64>) -> Vec<&'a str> {
    let len = lines.len() as f64;
    let index = |value: f64| {
        let value = if value.is_nan() { 0.0 } else { value.trunc() };
        (if value < 0.0 { (len + value).max(0.0) } else { value.min(len) }) as usize
    };
    let (from, to) = (index(start), end.map_or(lines.len(), index));
    if from < to { lines[from..to].to_vec() } else { Vec::new() }
}

/// The `read` tool before bounded reads: it decodes the whole file, splits it into lines, and truncates the selection.
fn reference_read(
    bytes: &[u8],
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
) -> Result<ToolExecutionResult, String> {
    if let Some(mime) = detect_supported_image_mime_type(bytes) {
        let message = format!("Read image file [{mime}].");
        return Ok(ToolExecutionResult {
            output: Some(vec![ContentBlock::Image { data: to_base64(bytes), mime_type: mime.into() }]),
            diagnostics: Some(vec![ToolDiagnostic::new(DiagnosticSeverity::Info, message).with_code("image")]),
            ..Default::default()
        });
    }
    let decoded = String::from_utf8_lossy(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes)).into_owned();
    let all: Vec<&str> = decoded.split('\n').collect();
    let total = all.len() as f64;
    let start = match offset {
        Some(offset) if offset != 0.0 && !offset.is_nan() => (offset - 1.0).max(0.0),
        _ => 0.0,
    };
    let start_display = js(start + 1.0);
    if start >= total {
        let offset = offset.map_or("undefined".into(), js);
        return Err(format!("Offset {offset} is beyond end of file ({} lines total)", js(total)));
    }
    let (selected, user_limited) = match limit {
        Some(limit) => {
            let end = (start + limit).min(total);
            (slice(&all, start, Some(end)).join("\n"), Some(end - start))
        }
        None => (slice(&all, start, None).join("\n"), None),
    };
    let truncation = truncate_head(&selected, Default::default());
    let mut diagnostics = Vec::new();
    let mut output = truncation.content.clone();
    let mut details = None;
    let mut fields = serde_json::to_value(&truncation).unwrap();
    fields.as_object_mut().unwrap().remove("content");
    let max = format_size(DEFAULT_MAX_BYTES as u64);
    if truncation.first_line_exceeds_limit {
        let line = if start.trunc() == start { all.get(start as usize).copied().unwrap_or("") } else { "" };
        let end = character_end(line.as_bytes(), DEFAULT_MAX_BYTES).min(line.len());
        output = line[..end].to_string();
        let message = format!(
            "Line {start_display} is {}, exceeds the {max} limit; showing its first {}. Use bash: sed -n '{start_display}p' {path} | tail -c +{}",
            format_size(line.len() as u64),
            format_size(end as u64),
            end + 1
        );
        diagnostics.push(ToolDiagnostic::new(DiagnosticSeverity::Warn, message).with_code("truncated"));
        (fields["outputBytes"], fields["outputLines"]) = (json!(end), json!(1));
        details = Some(json!({ "truncation": fields }));
    } else if truncation.truncated {
        let end_display = start + 1.0 + truncation.output_lines as f64 - 1.0;
        let limit_text =
            if truncation.truncated_by == Some(TruncatedBy::Lines) { String::new() } else { format!(" ({max} limit)") };
        let message = format!(
            "Showing lines {start_display}-{} of {}{limit_text}. Use offset={} to continue.",
            js(end_display),
            js(total),
            js(end_display + 1.0)
        );
        diagnostics.push(ToolDiagnostic::new(DiagnosticSeverity::Info, message).with_code("truncated"));
        details = Some(json!({ "truncation": fields }));
    } else if let Some(limited) = user_limited.filter(|limited| start + limited < total) {
        let message = format!(
            "{} more lines in file. Use offset={} to continue.",
            js(total - (start + limited)),
            js(start + limited + 1.0)
        );
        diagnostics.push(ToolDiagnostic::new(DiagnosticSeverity::Info, message));
    }
    Ok(ToolExecutionResult {
        output: Some(if output.is_empty() { Vec::new() } else { vec![ContentBlock::text(output)] }),
        details: details.map(Into::into),
        diagnostics: Some(diagnostics),
        ..Default::default()
    })
}

/// Upstream's mulberry32 generator.
fn random(seed: u32) -> impl FnMut() -> f64 {
    let mut state = seed;
    move || {
        state = state.wrapping_add(0x6d2b79f5);
        let mut t = state;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }
}

const PIECES: [&[u8]; 11] = [
    &[0x0a],
    &[0x0a],
    &[0x61],
    &[0x62, 0x63, 0x64, 0x65],
    &[0xef, 0xbb, 0xbf],
    &[0xc3, 0xa9],
    &[0xe2, 0x82, 0xac],
    &[0xf0, 0x9f, 0x98, 0x80],
    &[0xe2, 0x82],
    &[0xff],
    &[0x0d, 0x0a],
];

/// Mostly small files, some over the line limit, some over the byte limit, some with one huge line.
fn random_file(next: &mut impl FnMut() -> f64) -> Vec<u8> {
    let kind = next();
    if kind < 0.1 {
        return [&[0xef, 0xbb, 0xbf][..], "x\n".repeat(2100).as_bytes()].concat();
    }
    if kind < 0.2 {
        let mut line = vec![0x61; DEFAULT_MAX_BYTES + 200 + (next() * 400.0) as usize];
        line[DEFAULT_MAX_BYTES - 1 + (next() * 3.0) as usize] = 0xc3;
        line.extend([0x0a, 0x62]);
        return line;
    }
    if kind < 0.3 {
        return format!("{}\n", "0123456789".repeat(30)).repeat(200).into_bytes();
    }
    let pieces = (next() * 80.0) as usize;
    (0..pieces).flat_map(|_| PIECES[(next() * PIECES.len() as f64) as usize].to_vec()).collect()
}

// Upstream also draws NaN for the offset and the limit; JSON arguments cannot carry NaN, so it is not drawn here.
const OFFSETS: [Option<f64>; 11] = [
    None,
    Some(0.0),
    Some(1.0),
    Some(2.0),
    Some(3.0),
    Some(2.5),
    Some(-4.0),
    Some(50.0),
    Some(2001.0),
    Some(3000.0),
    Some(1e20),
];
const LIMITS: [Option<f64>; 11] = [
    None,
    Some(0.0),
    Some(1.0),
    Some(2.0),
    Some(-3.0),
    Some(-1e20),
    Some(1.5),
    Some(7.0),
    Some(2000.0),
    Some(2500.0),
    Some(1e20),
];

async fn actual(
    env: &Arc<LocalExecutionEnv>,
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
) -> Result<ToolExecutionResult, String> {
    let mut call = json!({ "path": path });
    if let Some(offset) = offset {
        call["offset"] = json!(offset);
    }
    if let Some(limit) = limit {
        call["limit"] = json!(limit);
    }
    let api = FakeApi::new(Some(env.clone()));
    execute_read(&ReadToolOptions::default(), args(call), api, ctx()).await.map_err(|error| error.to_string())
}

fn local(dir: &tempfile::TempDir) -> Arc<LocalExecutionEnv> {
    Arc::new(LocalExecutionEnv::new(LocalEnvOptions::new(dir.path().to_str().unwrap())))
}

#[tokio::test]
async fn returns_exactly_what_reading_the_whole_file_returned() {
    let dir = tempfile::tempdir().unwrap();
    let env = local(&dir);
    assert_eq!(random(1)(), 0.6270739405881613, "the generator draws upstream's sequence");
    for seed in 1..=400 {
        let mut next = random(seed);
        let file = random_file(&mut next);
        std::fs::write(dir.path().join("f.txt"), &file).unwrap();
        for _ in 0..4 {
            let offset = OFFSETS[(next() * OFFSETS.len() as f64) as usize];
            let limit = LIMITS[(next() * LIMITS.len() as f64) as usize];
            let expected = reference_read(&file, "f.txt", offset, limit);
            let got = actual(&env, "f.txt", offset, limit).await;
            assert_eq!(got, expected, "seed {seed} offset {offset:?} limit {limit:?}");
        }
    }
}

#[tokio::test]
async fn detects_animated_pngs_whose_actl_chunk_lies_far_beyond_the_header() {
    let dir = tempfile::tempdir().unwrap();
    let chunk = |kind: &str, length: u32| -> Vec<u8> {
        [&length.to_be_bytes()[..], kind.as_bytes(), &vec![0; length as usize + 4]].concat()
    };
    let png = [
        vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a],
        chunk("IHDR", 13),
        chunk("iCCP", 200_000),
        chunk("acTL", 8),
        chunk("IDAT", 10),
    ]
    .concat();
    std::fs::write(dir.path().join("a.png"), &png).unwrap();
    let got = actual(&local(&dir), "a.png", None, None).await;
    assert_eq!(got, reference_read(&png, "a.png", None, None));
}
