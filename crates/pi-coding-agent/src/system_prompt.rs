//! System prompt sections, ported from upstream `core/system-prompt.ts`. The prompt is a
//! leading [`SystemMessage`] whose named `sections` later system messages can patch.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::bail;
use pi_ai::transcript::system_message_text;
use pi_ai::{JsonObject, SystemMessage, UserContent};
use serde_json::Value;

use crate::config::ContextFile;
use crate::resources::Skill;
use crate::resources::context_entries::{ContextEntry, ContextExposure, SKILL_KIND, entry_exposure, skill_exposure};
use crate::resources::loader::LoadedResources;
use crate::resources::skills::{SkillFileReader, escape_xml, format_skills_for_prompt};
use crate::tools::{DEFAULT_TOOLS, tool_info};
use pi_codemode::TOOL_SEARCH_TOOL_NAME;

const PREAMBLE: &str = "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.";
const DOCS: &str = "Pi documentation (read only when the user asks about pi itself): this is pi-rust, a Rust port of the pi coding agent. See https://github.com/yannelli/pi-rust for usage, configuration, skills, prompt templates, and supported providers.";

/// Ordered `(name, text)` sections. `preamble` is untagged; every other section is wrapped
/// in a tag of its name so later updates can name it.
pub type SystemPromptSections = Vec<(String, String)>;

#[derive(Clone, Debug)]
pub struct BuildSystemPromptOptions {
    /// Replaces the preamble, tool list, rules, and docs sections (`SYSTEM.md`, `--system-prompt`).
    pub custom_prompt: Option<String>,
    /// Exact full prompt with no sections, as a `before_agent_start` handler sets it.
    pub force_system_prompt: Option<String>,
    pub selected_tools: Vec<String>,
    /// Selected tools whose declarations requests leave out; the tool list and rules skip them.
    pub hidden_tools: Vec<String>,
    pub tool_snippets: HashMap<String, String>,
    pub tool_guidelines: HashMap<String, Vec<String>>,
    pub prompt_guidelines: Vec<String>,
    /// `APPEND_SYSTEM.md` and `--append-system-prompt` text, placed before project context.
    pub append_system_prompt: String,
    /// Extra sections by tag name; a built-in name replaces that section in place.
    pub sections: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
    /// Context entries extensions contribute, such as agent configs.
    pub context_entries: Vec<ContextEntry>,
    /// The `contextExposure` setting. Deferred items stay out while `tool_search` is declared.
    pub context_exposure: BTreeMap<String, ContextExposure>,
}

impl BuildSystemPromptOptions {
    /// The default tools (`read`, `bash`, `edit`, `write`) with no snippets, context, or skills.
    pub fn new(cwd: &Path) -> Self {
        Self {
            custom_prompt: None,
            force_system_prompt: None,
            selected_tools: DEFAULT_TOOLS.iter().map(|name| name.to_string()).collect(),
            hidden_tools: Vec::new(),
            tool_snippets: HashMap::new(),
            tool_guidelines: HashMap::new(),
            prompt_guidelines: Vec::new(),
            append_system_prompt: String::new(),
            sections: Vec::new(),
            cwd: cwd.to_path_buf(),
            context_files: Vec::new(),
            skills: Vec::new(),
            context_entries: Vec::new(),
            context_exposure: BTreeMap::new(),
        }
    }

    /// Take `SYSTEM.md`, `APPEND_SYSTEM.md`, context files, and skills from a resource loader.
    pub fn with_resources(mut self, resources: &LoadedResources) -> Self {
        self.custom_prompt = resources.system_prompt.clone();
        self.append_system_prompt = resources.append_system_prompt_text();
        self.context_files = resources.context_files.clone();
        self.skills = resources.skills.clone();
        self.context_entries = resources.context_entries.clone();
        self
    }

    /// Fill missing snippets and guidelines of selected built-in tools from their tool info.
    pub fn with_builtin_tool_info(mut self) -> Self {
        for name in &self.selected_tools {
            let Some(info) = tool_info(name) else { continue };
            if !info.snippet.is_empty() {
                self.tool_snippets.entry(name.clone()).or_insert_with(|| info.snippet.to_string());
            }
            self.tool_guidelines
                .entry(name.clone())
                .or_insert_with(|| info.guidelines.iter().map(|rule| rule.to_string()).collect());
        }
        self
    }
}

fn is_valid_section_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

fn build_rules(selected: &[&String], guidelines: &HashMap<String, Vec<String>>, extra: &[String]) -> String {
    let mut rules: Vec<String> = Vec::new();
    let mut add = |rule: &str| {
        let rule = rule.trim();
        if !rule.is_empty() && !rules.iter().any(|existing| existing == rule) {
            rules.push(rule.to_string());
        }
    };
    let has = |name: &str| selected.iter().any(|tool| *tool == name);
    let (bash, powershell) = (has("bash"), has("powershell"));
    if (bash || powershell) && !has("grep") && !has("find") && !has("ls") {
        add(match (bash, powershell) {
            (true, true) => "Use bash or PowerShell for file operations like listing, searching, and finding files",
            (false, true) => "Use PowerShell for file operations like listing, searching, and finding files",
            _ => "Use bash for file operations like ls, rg, find",
        });
    }
    for name in selected {
        guidelines.get(name.as_str()).into_iter().flatten().for_each(|rule| add(rule));
    }
    extra.iter().for_each(|rule| add(rule));
    add("Be concise in your responses");
    add("Show file paths clearly when working with files");
    rules.iter().map(|rule| format!("- {rule}")).collect::<Vec<_>>().join("\n")
}

fn render_project_context(files: &[ContextFile]) -> String {
    let mut parts = vec!["Project-specific instructions and guidelines:".to_string()];
    parts.extend(files.iter().map(|file| {
        format!("<project_instructions path=\"{}\">\n{}\n</project_instructions>", file.path.display(), file.content)
    }));
    parts.join("\n\n")
}

fn render_context_entries(entries: &[&ContextEntry]) -> String {
    let mut lines = vec![
        "Extensions provide these entries. Use one when the task matches its description.".to_string(),
        String::new(),
        "<available_context>".to_string(),
    ];
    for entry in entries {
        lines.push("  <entry>".into());
        lines.push(format!("    <kind>{}</kind>", escape_xml(&entry.kind)));
        lines.push(format!("    <name>{}</name>", escape_xml(&entry.name)));
        lines.push(format!("    <description>{}</description>", escape_xml(&entry.description)));
        if let Some(usage) = entry.usage.as_deref().map(str::trim).filter(|usage| !usage.is_empty()) {
            lines.push(format!("    <usage>{}</usage>", escape_xml(usage)));
        }
        if let Some(path) = &entry.path {
            lines.push(format!("    <location>{}</location>", escape_xml(&path.display().to_string())));
        }
        lines.push("  </entry>".into());
    }
    lines.push("</available_context>".into());
    lines.join("\n")
}

/// The hint that replaces deferred items: how many of each kind `tool_search` can find.
fn render_deferred_context(counts: &BTreeMap<&str, usize>) -> String {
    let counts: Vec<String> =
        counts.iter().map(|(kind, count)| format!("{count} {kind}{}", if *count == 1 { "" } else { "s" })).collect();
    format!(
        "More context is available but not listed here: {}. Find it with `tool_search` using words from the task. A match returns its description and how to load or use it.",
        counts.join(", ")
    )
}

fn set_section(sections: &mut SystemPromptSections, name: &str, text: String) {
    match sections.iter_mut().find(|(existing, _)| existing == name) {
        Some(section) => section.1 = text,
        None => sections.push((name.to_string(), text)),
    }
}

/// The ordered, independently replaceable sections of the structured system prompt.
pub fn build_system_prompt_sections(options: &BuildSystemPromptOptions) -> anyhow::Result<SystemPromptSections> {
    for (name, _) in &options.sections {
        if !is_valid_section_name(name) || name == "preamble" {
            bail!("Invalid system prompt section name: {name}");
        }
    }
    let declared: Vec<&String> =
        options.selected_tools.iter().filter(|name| !options.hidden_tools.contains(name)).collect();
    let mut sections: SystemPromptSections = Vec::new();
    match options.custom_prompt.as_deref().filter(|prompt| !prompt.is_empty()) {
        Some(prompt) => sections.push(("preamble".into(), prompt.into())),
        None => {
            sections.push(("preamble".into(), PREAMBLE.into()));
            let visible: Vec<String> = declared
                .iter()
                .filter_map(|name| {
                    options.tool_snippets.get(name.as_str()).filter(|s| !s.is_empty()).map(|s| format!("- {name}: {s}"))
                })
                .collect();
            let tools = if visible.is_empty() { "(none)".to_string() } else { visible.join("\n") };
            let tools = format!(
                "{tools}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project."
            );
            sections.push(("tools".into(), tools));
            sections
                .push(("rules".into(), build_rules(&declared, &options.tool_guidelines, &options.prompt_guidelines)));
            sections.push(("docs".into(), DOCS.into()));
        }
    }
    if !options.append_system_prompt.is_empty() {
        sections.push(("addendum".into(), options.append_system_prompt.clone()));
    }
    if !options.context_files.is_empty() {
        sections.push(("project_context".into(), render_project_context(&options.context_files)));
    }
    let readers = [("read", SkillFileReader::Read), ("bash", SkillFileReader::Bash)];
    let reader = readers.iter().find(|(name, _)| declared.iter().any(|tool| tool == name)).map(|(_, reader)| *reader);
    let reader = reader.or_else(|| {
        let reachable = readers.iter().any(|(name, _)| options.selected_tools.iter().any(|tool| tool == name));
        reachable.then_some(SkillFileReader::Indirect)
    });
    let deferring = declared.iter().any(|tool| *tool == TOOL_SEARCH_TOOL_NAME);
    let is_deferred = |exposure: ContextExposure| deferring && exposure == ContextExposure::Deferred;
    let mut deferred: BTreeMap<&str, usize> = BTreeMap::new();
    let mut listed_skills = Vec::new();
    for skill in &options.skills {
        if !is_deferred(skill_exposure(skill, &options.context_exposure)) {
            listed_skills.push(skill.clone());
        } else if !skill.disable_model_invocation && reader.is_some() {
            *deferred.entry(SKILL_KIND).or_default() += 1;
        }
    }
    if let Some(reader) = reader.filter(|_| !listed_skills.is_empty()) {
        let skills = format_skills_for_prompt(&listed_skills, reader).trim().to_string();
        if !skills.is_empty() {
            sections.push(("skills".into(), skills));
        }
    }
    let mut listed_entries = Vec::new();
    for entry in &options.context_entries {
        if is_deferred(entry_exposure(entry, &options.context_exposure)) {
            *deferred.entry(entry.kind.as_str()).or_default() += 1;
        } else {
            listed_entries.push(entry);
        }
    }
    if !listed_entries.is_empty() {
        sections.push(("context_entries".into(), render_context_entries(&listed_entries)));
    }
    if !deferred.is_empty() {
        sections.push(("deferred_context".into(), render_deferred_context(&deferred)));
    }
    sections.push(("cwd".into(), options.cwd.display().to_string().replace('\\', "/")));
    for (name, text) in &options.sections {
        if !text.is_empty() {
            set_section(&mut sections, name, text.clone());
        }
    }
    Ok(sections
        .into_iter()
        .map(|(name, text)| {
            let text = if name == "preamble" { text } else { format!("<{name}>\n{text}\n</{name}>") };
            (name, text)
        })
        .collect())
}

/// Sections as the `sections` object of a [`SystemMessage`].
pub fn sections_to_json(sections: &SystemPromptSections) -> JsonObject {
    sections.iter().map(|(name, text)| (name.clone(), Value::String(text.clone()))).collect()
}

/// A forced prompt is opaque content with no sections; otherwise the sections carry the prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemPromptState {
    pub content: String,
    pub sections: Option<SystemPromptSections>,
}

pub fn build_system_prompt_state(options: &BuildSystemPromptOptions) -> anyhow::Result<SystemPromptState> {
    if let Some(forced) = &options.force_system_prompt {
        return Ok(SystemPromptState { content: forced.clone(), sections: None });
    }
    Ok(SystemPromptState { content: String::new(), sections: Some(build_system_prompt_sections(options)?) })
}

/// The leading system message of a transcript for `options`.
pub fn build_system_message(options: &BuildSystemPromptOptions, timestamp: i64) -> anyhow::Result<SystemMessage> {
    let state = build_system_prompt_state(options)?;
    Ok(SystemMessage {
        content: UserContent::Text(state.content),
        sections: state.sections.as_ref().map(sections_to_json),
        tools_added: None,
        tools_removed: None,
        timestamp,
    })
}

/// The prompt text exactly as the transcript's system message replays it.
pub fn render_system_prompt(options: &BuildSystemPromptOptions) -> anyhow::Result<String> {
    Ok(system_message_text(&build_system_message(options, 0)?))
}

/// A `sections` patch from the sections the model has to the wanted ones (`null` removes),
/// or `None` when nothing changed.
pub fn diff_system_prompt_sections(previous: &JsonObject, current: &SystemPromptSections) -> Option<JsonObject> {
    let mut patch = JsonObject::new();
    for (name, text) in current {
        if previous.get(name).and_then(Value::as_str) != Some(text.as_str()) {
            patch.insert(name.clone(), Value::String(text.clone()));
        }
    }
    for name in previous.keys() {
        if !current.iter().any(|(existing, _)| existing == name) {
            patch.insert(name.clone(), Value::Null);
        }
    }
    (!patch.is_empty()).then_some(patch)
}

/// Borrowed options of the original Rust API, kept for current callers.
#[derive(Clone, Debug)]
pub struct SystemPromptOptions<'a> {
    pub custom_prompt: Option<&'a str>,
    pub append_prompt: Option<&'a str>,
    pub selected_tools: &'a [String],
    pub context_files: &'a [ContextFile],
    pub cwd: &'a Path,
}

impl<'a> SystemPromptOptions<'a> {
    pub fn new(cwd: &'a Path) -> Self {
        Self { custom_prompt: None, append_prompt: None, selected_tools: &[], context_files: &[], cwd }
    }
}

impl From<&SystemPromptOptions<'_>> for BuildSystemPromptOptions {
    fn from(options: &SystemPromptOptions<'_>) -> Self {
        let mut built = Self::new(options.cwd);
        built.custom_prompt = options.custom_prompt.map(String::from);
        built.append_system_prompt = options.append_prompt.unwrap_or_default().to_string();
        built.selected_tools = options.selected_tools.to_vec();
        built.context_files = options.context_files.to_vec();
        built.with_builtin_tool_info()
    }
}

/// Sections for the original options, with built-in tool snippets and guidelines.
pub fn build_sections(options: &SystemPromptOptions<'_>) -> Vec<(String, String)> {
    build_system_prompt_sections(&options.into()).unwrap_or_default()
}

/// The flat prompt text for the original options.
pub fn build_system_prompt(options: &SystemPromptOptions<'_>) -> String {
    render_system_prompt(&options.into()).unwrap_or_default()
}
