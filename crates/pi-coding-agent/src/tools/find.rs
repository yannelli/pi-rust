//! `find` tool: native glob search that respects .gitignore, ported from the fd-backed tool.

use std::path::{Component, MAIN_SEPARATOR, Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use pi_agent::{AgentTool, AgentToolResult, ToolUpdateCallback};
use pi_ai::ContentBlock;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::grep::{build_walker, to_slash_relative};
use super::operations::{FindGlobOptions, FindOperations, FindToolOptions};
use super::path_utils::resolve_to_cwd;
use super::truncate::{DEFAULT_MAX_BYTES, TruncationOptions, format_size, truncate_head};
use super::{as_count, check_aborted, details_or_none, parse_args};

const DEFAULT_LIMIT: i64 = 1000;

pub struct FindTool {
    cwd: PathBuf,
    description: String,
    operations: Option<Arc<dyn FindOperations>>,
}

#[derive(Deserialize)]
struct FindArgs {
    pattern: String,
    path: Option<String>,
    limit: Option<f64>,
}

impl FindTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_options(cwd, FindToolOptions::default())
    }

    /// Custom operations replace the native search; without them the tool walks the local tree.
    pub fn with_options(cwd: PathBuf, options: FindToolOptions) -> Self {
        let description = format!(
            "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} results or {}KB (whichever is hit first).",
            DEFAULT_MAX_BYTES / 1024
        );
        Self { cwd, description, operations: options.operations }
    }
}

fn inside_git_repo(path: &Path) -> bool {
    path.ancestors().any(|dir| dir.join(".git").exists())
}

fn ignore_set(patterns: &[String]) -> anyhow::Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(GlobBuilder::new(pattern).literal_separator(true).build()?);
    }
    Ok(builder.build()?)
}

/// Native fd-style glob search under `search_path`: relative slash paths, `/` after directories.
pub(crate) fn native_glob(
    pattern: &str,
    search_path: &Path,
    limit: usize,
    ignore: &[String],
    cancel: &CancellationToken,
) -> anyhow::Result<Vec<String>> {
    // Like fd --glob: match the basename, or the full path when the pattern contains '/'.
    let full_path = pattern.contains('/');
    let pattern = if full_path && !(pattern.starts_with('/') || pattern.starts_with("**/") || pattern == "**") {
        format!("**/{pattern}")
    } else {
        pattern.to_string()
    };
    let matcher = GlobBuilder::new(&pattern).literal_separator(true).build()?.compile_matcher();
    let ignore = ignore_set(ignore)?;

    let mut results = Vec::new();
    for entry in build_walker(search_path, inside_git_repo(search_path), ".fdignore").build() {
        if cancel.is_cancelled() {
            bail!("Operation aborted");
        }
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 || ignore.is_match(entry.path()) {
            continue;
        }
        let matched =
            if full_path { matcher.is_match(entry.path()) } else { matcher.is_match(Path::new(entry.file_name())) };
        if !matched {
            continue;
        }
        let mut display = to_slash_relative(entry.path(), search_path);
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            display.push('/');
        }
        results.push(display);
        if results.len() >= limit {
            break;
        }
    }
    Ok(results)
}

/// `path` relative to `base`, with `..` for the parts of `base` it does not share.
fn relative_path(path: &Path, base: &Path) -> PathBuf {
    let path: Vec<Component> = path.components().collect();
    let base: Vec<Component> = base.components().collect();
    let common = path.iter().zip(&base).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..base.len() {
        out.push("..");
    }
    for component in &path[common..] {
        out.push(component.as_os_str());
    }
    out
}

/// A result of custom find operations relative to the search root, with `/` separators and one
/// trailing `/` for directories (upstream `relativizeFindResultPath`).
pub fn relativize_find_result_path(result_path: &str, search_path: &Path) -> String {
    let had_trailing_separator =
        result_path.ends_with(MAIN_SEPARATOR) || (MAIN_SEPARATOR == '\\' && result_path.ends_with('/'));
    let path = Path::new(result_path);
    let relative = if path.is_absolute() {
        relative_path(path, search_path).to_string_lossy().into_owned()
    } else {
        result_path.to_string()
    };
    let posix = relative.split(MAIN_SEPARATOR).collect::<Vec<_>>().join("/");
    if had_trailing_separator && !posix.ends_with('/') { format!("{posix}/") } else { posix }
}

fn run_find(args: FindArgs, cwd: &Path, cancel: &CancellationToken) -> anyhow::Result<AgentToolResult> {
    let search_path = resolve_to_cwd(args.path.as_deref().unwrap_or("."), cwd);
    let meta = std::fs::metadata(&search_path).map_err(|_| anyhow!("Path not found: {}", search_path.display()))?;
    if !meta.is_dir() {
        bail!("Not a directory: {}", search_path.display());
    }
    let limit = as_count(args.limit).unwrap_or(DEFAULT_LIMIT).max(1) as usize;
    let results = native_glob(&args.pattern, &search_path, limit, &[], cancel)?;
    let more = format!(". Use limit={} for more, or refine pattern", limit * 2);
    Ok(find_result(&results, limit, &more))
}

/// Find through custom operations, as upstream does when `operations.glob` is given.
async fn run_custom_find(
    ops: &dyn FindOperations,
    args: FindArgs,
    cwd: &Path,
    cancel: &CancellationToken,
) -> anyhow::Result<AgentToolResult> {
    let search_path = resolve_to_cwd(args.path.as_deref().unwrap_or("."), cwd);
    let limit = as_count(args.limit).unwrap_or(DEFAULT_LIMIT).max(1) as usize;
    if !ops.exists(&search_path).await.unwrap_or(false) {
        bail!("Path not found: {}", search_path.display());
    }
    check_aborted(cancel)?;
    let options = FindGlobOptions { ignore: vec!["**/node_modules/**".into(), "**/.git/**".into()], limit };
    let results = ops.glob(&args.pattern, &search_path, options).await?;
    check_aborted(cancel)?;
    let results: Vec<String> = results.iter().map(|path| relativize_find_result_path(path, &search_path)).collect();
    Ok(find_result(&results, limit, ""))
}

fn find_result(results: &[String], limit: usize, more: &str) -> AgentToolResult {
    if results.is_empty() {
        return AgentToolResult::text("No files found matching pattern");
    }
    let truncation = truncate_head(&results.join("\n"), TruncationOptions::bytes_only());
    let mut output = truncation.content.clone();
    let mut details = Map::new();
    let mut notices = Vec::new();
    if results.len() >= limit {
        notices.push(format!("{limit} results limit reached{more}"));
        details.insert("resultLimitReached".into(), json!(limit));
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert("truncation".into(), json!(truncation));
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    AgentToolResult {
        content: vec![ContentBlock::text(output)],
        details: details_or_none(details),
        ..Default::default()
    }
}

#[async_trait]
impl AgentTool for FindTool {
    fn annotations(&self) -> Option<&pi_agent::ToolAnnotations> {
        Some(&super::READ_ONLY_ANNOTATIONS)
    }

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

    fn prompt_snippet(&self) -> Option<&str> {
        super::builtin_snippet("find")
    }

    fn prompt_guidelines(&self) -> Vec<String> {
        super::builtin_guidelines("find")
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
        if let Some(ops) = &self.operations {
            return run_custom_find(ops.as_ref(), args, &self.cwd, &cancel).await;
        }
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || run_find(args, &cwd, &cancel)).await?
    }
}
