//! `grep` tool: native regex search that respects .gitignore, ported from the ripgrep-backed tool.

use std::path::{Path, PathBuf};

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

use super::path_utils::resolve_to_cwd;
use super::truncate::{
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, TruncationOptions, format_size, truncate_head, truncate_line,
};
use super::{as_count, details_or_none, parse_args};

const DEFAULT_LIMIT: i64 = 100;

pub struct GrepTool {
    cwd: PathBuf,
    description: String,
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
        let description = format!(
            "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} matches or {}KB (whichever is hit first). Long lines are truncated to {GREP_MAX_LINE_LENGTH} chars.",
            DEFAULT_MAX_BYTES / 1024
        );
        Self { cwd, description }
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

struct Search {
    regex: Regex,
    context: usize,
    limit: usize,
    lines_truncated: bool,
    match_count: usize,
    output: Vec<String>,
}

impl Search {
    fn line_text(&mut self, text: &str) -> String {
        let (text, truncated) = truncate_line(text, GREP_MAX_LINE_LENGTH);
        self.lines_truncated |= truncated;
        text
    }

    /// Search one file; returns true once the match limit is reached.
    fn file(&mut self, path: &Path, display: &str) -> bool {
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        if bytes.contains(&0) {
            return false;
        }
        let content = String::from_utf8_lossy(&bytes);
        let mut lines: Vec<&str> = content.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
        if content.ends_with('\n') {
            lines.pop();
        }
        for (idx, line) in lines.iter().enumerate() {
            if !self.regex.is_match(line) {
                continue;
            }
            self.match_count += 1;
            let number = idx + 1;
            let start = number.saturating_sub(self.context).max(1);
            let end = (number + self.context).min(lines.len());
            for current in start..=end {
                let text = self.line_text(&lines[current - 1].replace('\r', ""));
                if current == number {
                    self.output.push(format!("{display}:{current}: {text}"));
                } else {
                    self.output.push(format!("{display}-{current}- {text}"));
                }
            }
            if self.match_count >= self.limit {
                return true;
            }
        }
        false
    }
}

fn run_grep(args: GrepArgs, cwd: &Path, cancel: &CancellationToken) -> anyhow::Result<AgentToolResult> {
    let search_path = resolve_to_cwd(args.path.as_deref().unwrap_or("."), cwd);
    let meta = std::fs::metadata(&search_path).map_err(|_| anyhow!("Path not found: {}", search_path.display()))?;
    let is_dir = meta.is_dir();
    let pattern = if args.literal { regex::escape(&args.pattern) } else { args.pattern.clone() };
    let regex = RegexBuilder::new(&pattern).case_insensitive(args.ignore_case).build()?;
    let limit = as_count(args.limit).unwrap_or(DEFAULT_LIMIT).max(1) as usize;
    let context = as_count(args.context).unwrap_or(0).max(0) as usize;

    let mut walker = build_walker(&search_path, true, ".rgignore");
    if let Some(glob) = args.glob.as_deref().filter(|g| !g.is_empty()) {
        let mut overrides = OverrideBuilder::new(&search_path);
        overrides.add(glob)?;
        walker.overrides(overrides.build()?);
    }

    let mut search = Search { regex, context, limit, lines_truncated: false, match_count: 0, output: Vec::new() };
    let mut limit_reached = false;
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
            to_slash_relative(path, &search_path)
        } else {
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        };
        if search.file(path, &display) {
            limit_reached = true;
            break;
        }
    }

    if search.match_count == 0 {
        return Ok(AgentToolResult::text("No matches found"));
    }
    let truncation = truncate_head(&search.output.join("\n"), TruncationOptions::bytes_only());
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
    if search.lines_truncated {
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
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || run_grep(args, &cwd, &cancel)).await?
    }
}
