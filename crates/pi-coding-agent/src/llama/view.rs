//! Display data for the `/llama` view: model rows, sorting, search results, and the progress
//! bar (the non-rendering parts of upstream `extensions/llama/ui.ts`).

use std::sync::LazyLock;

use regex::Regex;

use super::client::{LlamaModelInfo, LlamaProgress, status};
use super::provider::configured_context_window;

/// Label of the list entry that opens the Hugging Face search.
pub const DOWNLOAD_LABEL: &str = "Download model…";
pub const DOWNLOAD_DESCRIPTION: &str = "Hugging Face owner/repository[:quant]";
pub const SEARCH_HINT: &str = "Model name or owner/repository[:quant]";
pub const SEARCH_MIN_CHARS_STATUS: &str = "Type at least 2 characters";
pub const SEARCHING_STATUS: &str = "Searching Hugging Face…";
pub const NO_RESULTS_STATUS: &str = "No GGUF models found";
/// Delay before a typed query is searched.
pub const SEARCH_DEBOUNCE_MS: u64 = 500;
const PROGRESS_BAR_WIDTH: usize = 40;

fn thousands(value: u64) -> String {
    if value >= 1000 { format!("{}k", (value as f64 / 1000.0).round() as u64) } else { value.to_string() }
}

/// The context size as `64k`: runtime or training context, else the command line.
pub fn context_label(model: &LlamaModelInfo) -> Option<String> {
    let context = model.meta.as_ref().and_then(|meta| meta.n_ctx.or(meta.n_ctx_train));
    context.filter(|value| *value > 0).or_else(|| configured_context_window(model)).map(thousands)
}

/// `loaded · 64k context`, the status for other states, nothing for unloaded models.
pub fn model_description(model: &LlamaModelInfo) -> String {
    let mut details = Vec::new();
    let loaded = model.is_loaded();
    if loaded {
        details.push("loaded".to_string());
    } else if !model.is_status(status::UNLOADED) {
        details.push(model.status.value.clone());
    }
    if loaded && let Some(context) = context_label(model) {
        details.push(format!("{context} context"));
    }
    details.join(" · ")
}

/// Loaded models first, then by id.
pub fn sort_models_for_display(models: &[LlamaModelInfo]) -> Vec<LlamaModelInfo> {
    let mut sorted = models.to_vec();
    sorted.sort_by(|left, right| {
        let loaded = right.is_status(status::LOADED).cmp(&left.is_status(status::LOADED));
        loaded.then_with(|| left.id.cmp(&right.id))
    });
    sorted
}

/// `1.2k`, `120k`, `3.4M`, `45M`.
pub fn compact_count(value: u64) -> String {
    let scaled = |divisor: f64, digits: i32, unit: &str| {
        format!("{}{unit}", super::client::to_fixed(value as f64 / divisor, digits))
    };
    match value {
        10_000_000.. => scaled(1_000_000.0, 0, "M"),
        1_000_000.. => scaled(1_000_000.0, 1, "M"),
        100_000.. => scaled(1_000.0, 0, "k"),
        1_000.. => scaled(1_000.0, 1, "k"),
        _ => value.to_string(),
    }
}

/// `owner/repository` or `owner/repository:quant`, taken as typed instead of a search result.
pub fn is_exact_model_query(query: &str) -> bool {
    static EXACT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[^/\s]+/[^:\s]+(?::[^\s:]+)?$").expect("valid pattern"));
    EXACT.is_match(query)
}

/// `owner/repository[:quant]` split at the first colon after the slash.
pub fn parse_hugging_face_model(value: &str) -> (String, Option<String>) {
    let start = value.find('/').map(|slash| slash + 1).unwrap_or(0);
    match value[start..].find(':') {
        Some(colon) => (value[..start + colon].to_string(), Some(value[start + colon + 1..].to_string())),
        None => (value.to_string(), None),
    }
}

/// The search line under the input: a hint, the searching notice, or a result count summary.
pub fn search_status(query: &str, results: Option<usize>) -> &'static str {
    match results {
        _ if query.chars().count() < 2 => SEARCH_MIN_CHARS_STATUS,
        None => SEARCHING_STATUS,
        Some(0) => NO_RESULTS_STATUS,
        Some(_) => "",
    }
}

/// A search result row: `owner/model  1.2k downloads`.
pub fn search_result_label(id: &str, downloads: u64) -> (String, String) {
    (id.to_string(), format!("{} downloads", compact_count(downloads)))
}

/// `█████───── 50%` for a ratio, clamped to 0..1.
pub fn progress_bar(ratio: f64) -> String {
    let clamped = ratio.clamp(0.0, 1.0);
    let filled = (clamped * PROGRESS_BAR_WIDTH as f64).round() as usize;
    let percent = (ratio * 100.0).round() as i64;
    format!("{}{} {percent}%", "█".repeat(filled), "─".repeat(PROGRESS_BAR_WIDTH - filled))
}

/// What the progress view shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProgressState {
    pub title: String,
    pub model: String,
    pub progress: LlamaProgress,
}
