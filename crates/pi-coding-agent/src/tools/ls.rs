//! `ls` tool: sorted directory listing with '/' suffix for directories, dotfiles included.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, ToolUpdateCallback};
use pi_ai::ContentBlock;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::operations::{LsOperations, LsToolOptions};
use super::path_utils::resolve_to_cwd;
use super::truncate::{DEFAULT_MAX_BYTES, TruncationOptions, format_size, truncate_head};
use super::{as_count, check_aborted, details_or_none, parse_args};

const DEFAULT_LIMIT: i64 = 500;

pub struct LsTool {
    cwd: PathBuf,
    description: String,
    operations: Arc<dyn LsOperations>,
}

#[derive(Deserialize)]
struct LsArgs {
    path: Option<String>,
    limit: Option<f64>,
}

impl LsTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_options(cwd, LsToolOptions::default())
    }

    pub fn with_options(cwd: PathBuf, options: LsToolOptions) -> Self {
        let description = format!(
            "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to {DEFAULT_LIMIT} entries or {}KB (whichever is hit first).",
            DEFAULT_MAX_BYTES / 1024
        );
        Self { cwd, description, operations: options.resolve() }
    }
}

#[async_trait]
impl AgentTool for LsTool {
    fn annotations(&self) -> Option<&pi_agent::ToolAnnotations> {
        Some(&super::READ_ONLY_ANNOTATIONS)
    }

    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list (default: current directory)" },
                "limit": { "type": "number", "description": "Maximum number of entries to return (default: 500)" }
            }
        })
    }

    fn prompt_snippet(&self) -> Option<&str> {
        super::builtin_snippet("ls")
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        super::builtin_guidelines("ls")
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let args: LsArgs = parse_args("ls", args)?;
        check_aborted(&cancel)?;
        let dir = resolve_to_cwd(args.path.as_deref().unwrap_or("."), &self.cwd);
        let limit = as_count(args.limit).unwrap_or(DEFAULT_LIMIT).max(0) as usize;

        let ops = &self.operations;
        if !ops.exists(&dir).await.unwrap_or(false) {
            bail!("Path not found: {}", dir.display());
        }
        if !ops.is_directory(&dir).await.map_err(|_| anyhow!("Path not found: {}", dir.display()))? {
            bail!("Not a directory: {}", dir.display());
        }
        let mut entries = ops.read_dir(&dir).await.map_err(|e| anyhow!("Cannot read directory: {e}"))?;
        entries.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b)));

        let mut results = Vec::new();
        let mut limit_reached = false;
        for entry in entries {
            if results.len() >= limit {
                limit_reached = true;
                break;
            }
            check_aborted(&cancel)?;
            let Ok(is_dir) = ops.is_directory(&dir.join(&entry)).await else {
                continue;
            };
            results.push(if is_dir { format!("{entry}/") } else { entry });
        }

        if results.is_empty() {
            return Ok(AgentToolResult::text("(empty directory)"));
        }
        let truncation = truncate_head(&results.join("\n"), TruncationOptions::bytes_only());
        let mut output = truncation.content.clone();
        let mut details = Map::new();
        let mut notices = Vec::new();
        if limit_reached {
            notices.push(format!("{limit} entries limit reached. Use limit={} for more", limit * 2));
            details.insert("entryLimitReached".into(), json!(limit));
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
}
