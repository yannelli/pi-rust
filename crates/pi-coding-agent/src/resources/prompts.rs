//! Prompt templates: Markdown files that become `/name` commands. Ported from upstream
//! `core/prompt-templates.ts`.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde::Serialize;
use serde_json::Value;

use super::ResourceDiagnostic;
use super::brand::CONFIG_DIR_NAME;
use super::frontmatter::parse_frontmatter;
use super::paths::{absolute, file_kind, is_under_path, resolve_path, sorted_entries};
use super::source_info::{SourceInfo, SourceScope, create_synthetic_source_info};

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    pub content: String,
    pub source_info: SourceInfo,
    pub file_path: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadPromptTemplatesResult {
    pub templates: Vec<PromptTemplate>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// Split arguments on whitespace, keeping single- or double-quoted text together. There
/// is no escape character, and empty quotes produce no argument.
pub fn parse_command_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in text.chars() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{([0-9]+|ARGUMENTS|@):-([^}]*)\}|\$\{@:([0-9]+)(?::([0-9]+))?\}|\$(ARGUMENTS|@|[0-9]+)").unwrap()
});

fn positional<'a>(args: &'a [String], number: &str) -> Option<&'a str> {
    let index = number.parse::<usize>().ok()?.checked_sub(1)?;
    args.get(index).map(String::as_str)
}

/// Replace `$1`, `$@`, `$ARGUMENTS`, `${N:-default}`, `${@:-default}`, `${@:N}`, and
/// `${@:N:L}`. Argument and default values are inserted literally, never substituted again.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all = args.join(" ");
    PLACEHOLDER
        .replace_all(content, |captures: &Captures| {
            if let Some(target) = captures.get(1) {
                let value = match target.as_str() {
                    "@" | "ARGUMENTS" => Some(all.as_str()),
                    number => positional(args, number),
                };
                return value.filter(|value| !value.is_empty()).unwrap_or(&captures[2]).to_string();
            }
            if let Some(start) = captures.get(3) {
                let start = start.as_str().parse::<usize>().unwrap_or(usize::MAX).saturating_sub(1).min(args.len());
                let end = match captures.get(4) {
                    Some(length) => start.saturating_add(length.as_str().parse().unwrap_or(usize::MAX)).min(args.len()),
                    None => args.len(),
                };
                return args[start..end].join(" ");
            }
            match &captures[5] {
                "@" | "ARGUMENTS" => all.clone(),
                number => positional(args, number).unwrap_or_default().to_string(),
            }
        })
        .into_owned()
}

fn truncated_first_line(body: &str) -> String {
    let Some(line) = body.split('\n').find(|line| !line.trim().is_empty()) else { return String::new() };
    let units: Vec<u16> = line.encode_utf16().collect();
    if units.len() <= 60 {
        return line.to_string();
    }
    format!("{}...", String::from_utf16_lossy(&units[..60]))
}

/// Load one template. The name is the file name without `.md`; the description falls back
/// to the first non-empty line, cut at 60 characters.
pub fn load_template_from_file(path: &Path, source_info: SourceInfo) -> LoadPromptTemplatesResult {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => return diagnostic_only(ResourceDiagnostic::warning(error.to_string(), path)),
    };
    let parsed = match parse_frontmatter(&raw) {
        Ok(parsed) => parsed,
        Err(error) => return diagnostic_only(ResourceDiagnostic::warning(error.0, path)),
    };
    let file_name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let name = file_name.strip_suffix(".md").unwrap_or(&file_name).to_string();
    let mut description = parsed.frontmatter.get("description").and_then(Value::as_str).unwrap_or_default().to_string();
    if description.is_empty() {
        description = truncated_first_line(&parsed.body);
    }
    let argument_hint = parsed
        .frontmatter
        .get("argument-hint")
        .and_then(Value::as_str)
        .filter(|hint| !hint.is_empty())
        .map(String::from);
    let template =
        PromptTemplate { name, description, argument_hint, content: parsed.body, source_info, file_path: path.into() };
    LoadPromptTemplatesResult { templates: vec![template], diagnostics: Vec::new() }
}

fn diagnostic_only(diagnostic: ResourceDiagnostic) -> LoadPromptTemplatesResult {
    LoadPromptTemplatesResult { templates: Vec::new(), diagnostics: vec![diagnostic] }
}

/// Direct `.md` children of `dir`; subdirectories are not searched.
fn load_templates_from_dir(dir: &Path, source_info: &dyn Fn(&Path) -> SourceInfo) -> LoadPromptTemplatesResult {
    let mut result = LoadPromptTemplatesResult::default();
    for (name, path) in sorted_entries(dir) {
        if name.ends_with(".md") && file_kind(&path).is_some_and(|(is_file, _)| is_file) {
            let loaded = load_template_from_file(&path, source_info(&path));
            result.templates.extend(loaded.templates);
            result.diagnostics.extend(loaded.diagnostics);
        }
    }
    result
}

#[derive(Clone, Debug)]
pub struct LoadPromptTemplatesOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    /// Explicit template files or directories.
    pub prompt_paths: Vec<String>,
    /// Also load `<agent-dir>/prompts` and `<cwd>/.pi/prompts`.
    pub include_defaults: bool,
}

/// Load templates from the default directories and explicit paths. Duplicates are kept;
/// the resource loader reports name collisions.
pub fn load_prompt_templates(options: &LoadPromptTemplatesOptions) -> LoadPromptTemplatesResult {
    let cwd = absolute(&options.cwd);
    let global_dir = absolute(&options.agent_dir).join("prompts");
    let project_dir = cwd.join(CONFIG_DIR_NAME).join("prompts");
    let source_info = |path: &Path| -> SourceInfo {
        let display = path.display().to_string();
        if is_under_path(path, &global_dir) {
            return create_synthetic_source_info(&display, "local", Some(SourceScope::User), Some(&global_dir));
        }
        if is_under_path(path, &project_dir) {
            return create_synthetic_source_info(&display, "local", Some(SourceScope::Project), Some(&project_dir));
        }
        let base = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
        create_synthetic_source_info(&display, "local", None, Some(base))
    };
    let mut result = LoadPromptTemplatesResult::default();
    let mut add = |loaded: LoadPromptTemplatesResult| {
        result.templates.extend(loaded.templates);
        result.diagnostics.extend(loaded.diagnostics);
    };
    if options.include_defaults {
        add(load_templates_from_dir(&global_dir, &source_info));
        add(load_templates_from_dir(&project_dir, &source_info));
    }
    for raw in &options.prompt_paths {
        let path = resolve_path(raw, &cwd);
        match file_kind(&path) {
            Some((_, true)) => add(load_templates_from_dir(&path, &source_info)),
            Some((true, _)) if path.extension().is_some_and(|ext| ext == "md") => {
                add(load_template_from_file(&path, source_info(&path)))
            }
            _ => {}
        }
    }
    result
}

static TEMPLATE_COMMAND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)\A/(\S+)(?:\s+(.*))?\z").unwrap());

/// Expand `/name args` when `name` is a template; other text comes back unchanged.
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    let Some(captures) = TEMPLATE_COMMAND.captures(text) else { return text.to_string() };
    match templates.iter().find(|template| template.name == captures[1]) {
        Some(template) => {
            let args = parse_command_args(captures.get(2).map_or("", |m| m.as_str()));
            substitute_args(&template.content, &args)
        }
        None => text.to_string(),
    }
}
