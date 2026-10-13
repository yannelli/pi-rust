//! `grep` tool: native regex search that respects .gitignore, ported from the ripgrep-backed tool.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use pi_agent::{AgentTool, AgentToolResult, ToolUpdateCallback};
use pi_ai::ContentBlock;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::operations::{GrepOperations, GrepToolOptions};
use super::path_utils::resolve_to_cwd;
use super::truncate::{
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, TruncationOptions, format_size, truncate_head, truncate_line,
};
use super::{as_count, check_aborted, details_or_none, parse_args};

const DEFAULT_LIMIT: i64 = 100;

pub struct GrepTool {
    cwd: PathBuf,
    description: String,
    operations: Arc<dyn GrepOperations>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    literal: bool,
    context: Option<f64>,
    limit: Option<f64>,
}

impl GrepTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_options(cwd, GrepToolOptions::default())
    }

    pub fn with_options(cwd: PathBuf, options: GrepToolOptions) -> Self {
        let description = format!(
            "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} matches or {}KB (whichever is hit first). Long lines are truncated to {GREP_MAX_LINE_LENGTH} chars.",
            DEFAULT_MAX_BYTES / 1024
        );
        Self { cwd, description, operations: options.resolve() }
    }
}

/// Walker shared by grep and find: hidden files included, ignore files respected, `.git` skipped.
pub(crate) fn build_walker(root: &Path, require_git: bool, custom_ignore: &str) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .require_git(require_git)
        .add_custom_ignore_filename(custom_ignore)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| entry.file_name() != ".git");
    builder
}

pub(crate) fn to_slash_relative(path: &Path, base: &Path) -> String {
    let rel = path.strip_prefix(base).unwrap_or(path);
    rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

/// One matching line, formatted once the search finishes.
struct Match {
    path: PathBuf,
    display: String,
    line_number: usize,
    line_text: String,
}

struct Search {
    regex: Regex,
    limit: usize,
    matches: Vec<Match>,
}

impl Search {
    /// Search one file; returns true once the match limit is reached.
    fn file(&mut self, path: &Path, display: &str) -> bool {
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        if bytes.contains(&0) {
            return false;
        }
        let content = String::from_utf8_lossy(&bytes);
        for (idx, line) in split_lines(&content).into_iter().enumerate() {
            if !self.regex.is_match(line) {
                continue;
            }
            let line_text = line.replace('\r', "");
            self.matches.push(Match {
                path: path.to_path_buf(),
                display: display.to_string(),
                line_number: idx + 1,
                line_text,
            });
            if self.matches.len() >= self.limit {
                return true;
            }
        }
        false
    }
}

/// Lines without their `\r\n` or `\n` endings and without the empty line after a final newline.
fn split_lines(content: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = content.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Walk the local tree under `search_path` and collect up to `limit` matches.
fn search_files(
    args: &GrepArgs,
    search_path: &Path,
    is_dir: bool,
    limit: usize,
    cancel: &CancellationToken,
) -> anyhow::Result<(Vec<Match>, bool)> {
    let pattern = if args.literal { regex::escape(&args.pattern) } else { args.pattern.clone() };
    let regex = RegexBuilder::new(&pattern).case_insensitive(args.ignore_case).build()?;
    let mut walker = build_walker(search_path, true, ".rgignore");
    if let Some(glob) = args.glob.as_deref().filter(|g| !g.is_empty()) {
        let mut overrides = OverrideBuilder::new(search_path);
        overrides.add(glob)?;
        walker.overrides(overrides.build()?);
    }

    let mut search = Search { regex, limit, matches: Vec::new() };
    for entry in walker.build() {
        if cancel.is_cancelled() {
            bail!("Operation aborted");
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let display = if is_dir {
            to_slash_relative(path, search_path)
        } else {
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        };
        if search.file(path, &display) {
            return Ok((search.matches, true));
        }
    }
    Ok((search.matches, false))
}

/// Output lines of the matches; context lines come from the backend's `read_file`.
async fn format_matches(ops: &dyn GrepOperations, matches: &[Match], context: usize) -> (Vec<String>, bool) {
    let mut lines_truncated = false;
    let mut truncate = |text: &str| {
        let (text, truncated) = truncate_line(text, GREP_MAX_LINE_LENGTH);
        lines_truncated |= truncated;
        text
    };
    let mut cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
    let mut output = Vec::new();
    for m in matches {
        let number = m.line_number;
        if context == 0 {
            output.push(format!("{}:{number}: {}", m.display, truncate(&m.line_text)));
            continue;
        }
        if !cache.contains_key(&m.path) {
            let text = ops.read_file(&m.path).await.unwrap_or_default();
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            cache.insert(m.path.clone(), split_lines(&text).into_iter().map(str::to_string).collect());
        }
        let lines = &cache[&m.path];
        if lines.is_empty() {
            output.push(format!("{}:{number}: (unable to read file)", m.display));
            continue;
        }
        let start = number.saturating_sub(context).max(1);
        let end = (number + context).min(lines.len());
        for current in start..=end {
            let text = truncate(lines.get(current - 1).map_or("", String::as_str));
            if current == number {
                output.push(format!("{}:{current}: {text}", m.display));
            } else {
                output.push(format!("{}-{current}- {text}", m.display));
            }
        }
    }
    (output, lines_truncated)
}

async fn run_grep(
    ops: &dyn GrepOperations,
    args: GrepArgs,
    cwd: &Path,
    cancel: CancellationToken,
) -> anyhow::Result<AgentToolResult> {
    let search_path = resolve_to_cwd(args.path.as_deref().unwrap_or("."), cwd);
    let is_dir =
        ops.is_directory(&search_path).await.map_err(|_| anyhow!("Path not found: {}", search_path.display()))?;
    let limit = as_count(args.limit).unwrap_or(DEFAULT_LIMIT).max(1) as usize;
    let context = as_count(args.context).unwrap_or(0).max(0) as usize;
    let path = search_path.clone();
    let task_cancel = cancel.clone();
    let (matches, limit_reached) =
        tokio::task::spawn_blocking(move || search_files(&args, &path, is_dir, limit, &task_cancel)).await??;

    if matches.is_empty() {
        return Ok(AgentToolResult::text("No matches found"));
    }
    let (lines, lines_truncated) = format_matches(ops, &matches, context).await;
    check_aborted(&cancel)?;
    let truncation = truncate_head(&lines.join("\n"), TruncationOptions::bytes_only());
    let mut output = truncation.content.clone();
    let mut details = Map::new();
    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!("{limit} matches limit reached. Use limit={} for more, or refine pattern", limit * 2));
        details.insert("matchLimitReached".into(), json!(limit));
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert("truncation".into(), json!(truncation));
    }
    if lines_truncated {
        notices.push(format!("Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"));
        details.insert("linesTruncated".into(), json!(true));
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(AgentToolResult {
        content: vec![ContentBlock::text(output)],
        details: details_or_none(details),
        ..Default::default()
    })
}

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
                "path": { "type": "string", "description": "Directory or file to search (default: current directory)" },
                "glob": { "type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'" },
                "ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
                "literal": { "type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)" },
                "context": { "type": "number", "description": "Number of lines to show before and after each match (default: 0)" },
                "limit": { "type": "number", "description": "Maximum number of matches to return (default: 100)" }
            },
            "required": ["pattern"]
        })
    }

    fn prompt_snippet(&self) -> Option<&str> {
        super::builtin_snippet("grep")
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        super::builtin_guidelines("grep")
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let args: GrepArgs = parse_args("grep", args)?;
        if cancel.is_cancelled() {
            bail!("Operation aborted");
        }
        run_grep(self.operations.as_ref(), args, &self.cwd, cancel).await
    }
}
