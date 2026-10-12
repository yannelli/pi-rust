//! `find` tool: native glob search that respects .gitignore, ported from the fd-backed tool.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use globset::GlobBuilder;
use pi_agent::{AgentTool, AgentToolResult, ToolUpdateCallback};
use pi_ai::ContentBlock;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::grep::{build_walker, to_slash_relative};
use super::path_utils::resolve_to_cwd;
use super::truncate::{DEFAULT_MAX_BYTES, TruncationOptions, format_size, truncate_head};
use super::{as_count, details_or_none, parse_args};

const DEFAULT_LIMIT: i64 = 1000;

pub struct FindTool {
    cwd: PathBuf,
    description: String,
}

#[derive(Deserialize)]
struct FindArgs {
    pattern: String,
    path: Option<String>,
    limit: Option<f64>,
}

impl FindTool {
    pub fn new(cwd: PathBuf) -> Self {
        let description = format!(
            "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} results or {}KB (whichever is hit first).",
            DEFAULT_MAX_BYTES / 1024
        );
        Self { cwd, description }
    }
}

fn inside_git_repo(path: &Path) -> bool {
    path.ancestors().any(|dir| dir.join(".git").exists())
}

fn run_find(args: FindArgs, cwd: &Path, cancel: &CancellationToken) -> anyhow::Result<AgentToolResult> {
    let search_path = resolve_to_cwd(args.path.as_deref().unwrap_or("."), cwd);
    let meta = std::fs::metadata(&search_path).map_err(|_| anyhow!("Path not found: {}", search_path.display()))?;
    if !meta.is_dir() {
        bail!("Not a directory: {}", search_path.display());
    }
    let limit = as_count(args.limit).unwrap_or(DEFAULT_LIMIT).max(1) as usize;

    // Like fd --glob: match the basename, or the full path when the pattern contains '/'.
    let full_path = args.pattern.contains('/');
    let pattern =
        if full_path && !(args.pattern.starts_with('/') || args.pattern.starts_with("**/") || args.pattern == "**") {
            format!("**/{}", args.pattern)
        } else {
            args.pattern.clone()
        };
    let matcher = GlobBuilder::new(&pattern).literal_separator(true).build()?.compile_matcher();

    let mut results = Vec::new();
    for entry in build_walker(&search_path, inside_git_repo(&search_path), ".fdignore").build() {
        if cancel.is_cancelled() {
            bail!("Operation aborted");
        }
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let matched =
            if full_path { matcher.is_match(entry.path()) } else { matcher.is_match(Path::new(entry.file_name())) };
        if !matched {
            continue;
        }
        let mut display = to_slash_relative(entry.path(), &search_path);
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            display.push('/');
        }
        results.push(display);
        if results.len() >= limit {
            break;
        }
    }

    if results.is_empty() {
        return Ok(AgentToolResult::text("No files found matching pattern"));
    }
    let truncation = truncate_head(&results.join("\n"), TruncationOptions::bytes_only());
    let mut output = truncation.content.clone();
    let mut details = Map::new();
    let mut notices = Vec::new();
    if results.len() >= limit {
        notices.push(format!("{limit} results limit reached. Use limit={} for more, or refine pattern", limit * 2));
        details.insert("resultLimitReached".into(), json!(limit));
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert("truncation".into(), json!(truncation));
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
impl AgentTool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"
                },
                "path": { "type": "string", "description": "Directory to search in (default: current directory)" },
                "limit": { "type": "number", "description": "Maximum number of results (default: 1000)" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let args: FindArgs = parse_args("find", args)?;
        if cancel.is_cancelled() {
            bail!("Operation aborted");
        }
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || run_find(args, &cwd, &cancel)).await?
    }
}
