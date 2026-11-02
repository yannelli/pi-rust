//! Agent Skills discovery and prompt formatting, ported from upstream `core/skills.ts`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::brand::CONFIG_DIR_NAME;
use super::frontmatter::parse_frontmatter;
use super::paths::{IgnoreRules, absolute, canonicalize, file_kind, is_under_path, relative_posix, resolve_path};
use super::source_info::{SourceInfo, SourceScope, create_synthetic_source_info};
use super::{CollisionResourceType, ResourceDiagnostic};

pub const MAX_NAME_LENGTH: usize = 64;
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub source_info: SourceInfo,
    #[serde(default)]
    pub disable_model_invocation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
    /// The experimental `allowed-tools` list, split on whitespace when given as a string.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadSkillsResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// Agent Skills name rules: at most 64 of `a-z`, `0-9`, `-`, no edge or doubled hyphens.
pub fn validate_name(name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let length = name.encode_utf16().count();
    if length > MAX_NAME_LENGTH {
        errors.push(format!("name exceeds {MAX_NAME_LENGTH} characters ({length})"));
    }
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-') {
        errors.push("name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".into());
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".into());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".into());
    }
    errors
}

fn validate_description(description: Option<&Value>) -> Vec<String> {
    match description.and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() => {
            let length = text.encode_utf16().count();
            if length > MAX_DESCRIPTION_LENGTH {
                vec![format!("description exceeds {MAX_DESCRIPTION_LENGTH} characters ({length})")]
            } else {
                Vec::new()
            }
        }
        _ => vec!["description is required".into()],
    }
}

fn skill_source_info(file_path: &Path, base_dir: &Path, source: &str) -> SourceInfo {
    let path = file_path.display().to_string();
    match source {
        "user" => create_synthetic_source_info(&path, "local", Some(SourceScope::User), Some(base_dir)),
        "project" => create_synthetic_source_info(&path, "local", Some(SourceScope::Project), Some(base_dir)),
        "path" => create_synthetic_source_info(&path, "local", None, Some(base_dir)),
        other => create_synthetic_source_info(&path, other, None, Some(base_dir)),
    }
}

fn string_field(frontmatter: &Map<String, Value>, key: &str) -> Option<String> {
    frontmatter.get(key).and_then(Value::as_str).map(String::from)
}

fn allowed_tools(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) => text.split_whitespace().map(String::from).collect(),
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(String::from).collect(),
        _ => Vec::new(),
    }
}

/// Load one skill file. A `SKILL.md` without a description warns; other Markdown files
/// without one are not skills and load silently as nothing.
pub fn load_skill_from_file(file_path: &Path, source: &str) -> (Option<Skill>, Vec<ResourceDiagnostic>) {
    let mut diagnostics = Vec::new();
    let is_declared = file_path.file_name().is_some_and(|name| name == "SKILL.md");
    let raw = match std::fs::read_to_string(file_path) {
        Ok(raw) => raw,
        Err(error) => return (None, vec![ResourceDiagnostic::warning(error.to_string(), file_path)]),
    };
    let frontmatter = match parse_frontmatter(&raw) {
        Ok(parsed) => parsed.frontmatter,
        Err(error) => {
            if is_declared {
                diagnostics.push(ResourceDiagnostic::warning(error.0, file_path));
            }
            return (None, diagnostics);
        }
    };
    let description = frontmatter.get("description").and_then(Value::as_str).filter(|text| !text.trim().is_empty());
    if !is_declared && description.is_none() {
        return (None, diagnostics);
    }
    let skill_dir = file_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let parent_name = skill_dir.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    for error in validate_description(frontmatter.get("description")) {
        diagnostics.push(ResourceDiagnostic::warning(error, file_path));
    }
    let name = string_field(&frontmatter, "name").filter(|name| !name.is_empty()).unwrap_or(parent_name);
    for error in validate_name(&name) {
        diagnostics.push(ResourceDiagnostic::warning(error, file_path));
    }
    let Some(description) = description else { return (None, diagnostics) };
    let skill = Skill {
        name,
        description: description.to_string(),
        file_path: file_path.to_path_buf(),
        source_info: skill_source_info(file_path, &skill_dir, source),
        base_dir: skill_dir,
        disable_model_invocation: frontmatter.get("disable-model-invocation") == Some(&Value::Bool(true)),
        license: string_field(&frontmatter, "license"),
        compatibility: string_field(&frontmatter, "compatibility"),
        metadata: frontmatter.get("metadata").and_then(Value::as_object).cloned().unwrap_or_default(),
        allowed_tools: allowed_tools(frontmatter.get("allowed-tools")),
    };
    (Some(skill), diagnostics)
}

/// Load skills under `dir`: a directory holding `SKILL.md` is one skill and is not searched
/// further; otherwise direct `.md` children of `dir` load and subdirectories are searched.
pub fn load_skills_from_dir(dir: &Path, source: &str) -> LoadSkillsResult {
    let mut result = LoadSkillsResult::default();
    if dir.exists() {
        let mut rules = IgnoreRules::new(dir);
        load_dir(dir, source, true, &mut rules, dir, &mut result);
    }
    result
}

fn load_dir(
    dir: &Path,
    source: &str,
    include_root: bool,
    rules: &mut IgnoreRules,
    root: &Path,
    out: &mut LoadSkillsResult,
) {
    rules.add_dir(dir);
    let entries = super::paths::sorted_entries(dir);
    let skill_file = dir.join("SKILL.md");
    if entries.iter().any(|(name, _)| name == "SKILL.md")
        && file_kind(&skill_file).is_some_and(|(is_file, _)| is_file)
        && !rules.ignores(&relative_posix(root, &skill_file), false)
    {
        let (skill, diagnostics) = load_skill_from_file(&skill_file, source);
        out.skills.extend(skill);
        out.diagnostics.extend(diagnostics);
        return;
    }
    for (name, path) in entries {
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let Some((is_file, is_dir)) = file_kind(&path) else { continue };
        if rules.ignores(&relative_posix(root, &path), is_dir) {
            continue;
        }
        if is_dir {
            load_dir(&path, source, false, rules, root, out);
        } else if is_file && include_root && name.ends_with(".md") {
            let (skill, diagnostics) = load_skill_from_file(&path, source);
            out.skills.extend(skill);
            out.diagnostics.extend(diagnostics);
        }
    }
}

/// Which tool the skills section tells the model to load skill files with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillFileReader {
    Read,
    Bash,
    /// The reader is reachable only through another tool, so the hint names none.
    Indirect,
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// The `<available_skills>` listing; skills with `disable-model-invocation` are left out.
pub fn format_skills_for_prompt(skills: &[Skill], reader: SkillFileReader) -> String {
    let visible: Vec<&Skill> = skills.iter().filter(|skill| !skill.disable_model_invocation).collect();
    if visible.is_empty() {
        return String::new();
    }
    let hint = match reader {
        SkillFileReader::Read => "Use the read tool to load a skill's file when the task matches its description.",
        SkillFileReader::Bash => "Use bash to load a skill's file when the task matches its description.",
        SkillFileReader::Indirect => "Load a skill's file when the task matches its description.",
    };
    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        hint.to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for skill in visible {
        lines.push("  <skill>".into());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!("    <description>{}</description>", escape_xml(&skill.description)));
        lines.push(format!("    <location>{}</location>", escape_xml(&skill.file_path.display().to_string())));
        lines.push("  </skill>".into());
    }
    lines.push("</available_skills>".into());
    lines.join("\n")
}

#[derive(Clone, Debug)]
pub struct LoadSkillsOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    /// Explicit skill files or directories.
    pub skill_paths: Vec<String>,
    /// Also load `<agent-dir>/skills` and `<cwd>/.pi/skills`.
    pub include_defaults: bool,
}

#[derive(Default)]
struct SkillSet {
    result: LoadSkillsResult,
    real_paths: HashSet<PathBuf>,
    collisions: Vec<ResourceDiagnostic>,
}

impl SkillSet {
    fn add(&mut self, loaded: LoadSkillsResult) {
        self.result.diagnostics.extend(loaded.diagnostics);
        for skill in loaded.skills {
            let real_path = canonicalize(&skill.file_path);
            if self.real_paths.contains(&real_path) {
                continue;
            }
            match self.result.skills.iter().find(|existing| existing.name == skill.name) {
                Some(existing) => self.collisions.push(ResourceDiagnostic::collision(
                    CollisionResourceType::Skill,
                    format!("name \"{}\" collision", skill.name),
                    &skill.name,
                    &existing.file_path,
                    &skill.file_path,
                )),
                None => {
                    self.real_paths.insert(real_path);
                    self.result.skills.push(skill);
                }
            }
        }
    }
}

/// Load skills from every configured location. The first skill with a name wins.
pub fn load_skills(options: &LoadSkillsOptions) -> LoadSkillsResult {
    let cwd = absolute(&options.cwd);
    let agent_dir = absolute(&options.agent_dir);
    let user_dir = agent_dir.join("skills");
    let project_dir = cwd.join(CONFIG_DIR_NAME).join("skills");
    let mut set = SkillSet::default();
    if options.include_defaults {
        set.add(load_skills_from_dir(&user_dir, "user"));
        set.add(load_skills_from_dir(&project_dir, "project"));
    }
    let source_for = |path: &Path| match () {
        _ if !options.include_defaults && is_under_path(path, &user_dir) => "user",
        _ if !options.include_defaults && is_under_path(path, &project_dir) => "project",
        _ => "path",
    };
    for raw in &options.skill_paths {
        let path = resolve_path(raw, &cwd);
        let Some((is_file, is_dir)) = file_kind(&path) else {
            set.result.diagnostics.push(ResourceDiagnostic::warning("skill path does not exist", &path));
            continue;
        };
        if is_dir {
            set.add(load_skills_from_dir(&path, source_for(&path)));
        } else if is_file && path.extension().is_some_and(|ext| ext == "md") {
            let (skill, diagnostics) = load_skill_from_file(&path, source_for(&path));
            match skill {
                Some(skill) => set.add(LoadSkillsResult { skills: vec![skill], diagnostics }),
                None => set.result.diagnostics.extend(diagnostics),
            }
        } else {
            set.result.diagnostics.push(ResourceDiagnostic::warning("skill path is not a markdown file", &path));
        }
    }
    set.result.diagnostics.append(&mut set.collisions);
    set.result
}

/// Expand `/skill:name args` into a `<skill>` block followed by the arguments. Other text,
/// and unknown skill names, come back unchanged; a read or parse failure is an error.
pub fn expand_skill_command(text: &str, skills: &[Skill]) -> anyhow::Result<String> {
    let Some(rest) = text.strip_prefix("/skill:") else { return Ok(text.to_string()) };
    let (name, args) = match rest.find(' ') {
        Some(at) => (&rest[..at], rest[at + 1..].trim()),
        None => (rest, ""),
    };
    let Some(skill) = skills.iter().find(|skill| skill.name == name) else { return Ok(text.to_string()) };
    let content = std::fs::read_to_string(&skill.file_path)?;
    let body = parse_frontmatter(&content)?.body;
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name,
        skill.file_path.display(),
        skill.base_dir.display(),
        body.trim()
    );
    Ok(if args.is_empty() { block } else { format!("{block}\n\n{args}") })
}

/// A `<skill>` block from [`expand_skill_command`], split back into its parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSkillBlock {
    pub name: String,
    pub location: String,
    pub content: String,
    pub user_message: Option<String>,
}

static SKILL_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)\A<skill name="([^"]+)" location="([^"]+)">\n(.*?)\n</skill>(?:\n\n(.+))?\z"#).unwrap()
});

pub fn parse_skill_block(text: &str) -> Option<ParsedSkillBlock> {
    let captures = SKILL_BLOCK.captures(text)?;
    Some(ParsedSkillBlock {
        name: captures[1].to_string(),
        location: captures[2].to_string(),
        content: captures[3].to_string(),
        user_message: captures.get(4).map(|m| m.as_str().trim().to_string()).filter(|message| !message.is_empty()),
    })
}
