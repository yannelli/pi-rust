//! System prompt sections, ported from the TypeScript pi `buildSystemPromptSections`.

use std::path::Path;

use crate::config::ContextFile;
use crate::tools::tool_info;

const PREAMBLE: &str = "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.";

#[derive(Clone, Debug)]
pub struct SystemPromptOptions<'a> {
    /// Replaces the preamble, tool list, rules, and docs sections.
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

fn rules(selected: &[String]) -> String {
    let mut rules: Vec<String> = Vec::new();
    let mut add = |rule: &str| {
        let rule = rule.trim();
        if !rule.is_empty() && !rules.iter().any(|existing| existing == rule) {
            rules.push(rule.to_string());
        }
    };
    let has = |name: &str| selected.iter().any(|tool| tool == name);
    if has("bash") && !has("grep") && !has("find") && !has("ls") {
        add("Use bash for file operations like ls, rg, find");
    }
    for name in selected {
        for rule in tool_info(name).map(|info| info.guidelines).unwrap_or_default() {
            add(rule);
        }
    }
    add("Be concise in your responses");
    add("Show file paths clearly when working with files");
    rules.iter().map(|rule| format!("- {rule}")).collect::<Vec<_>>().join("\n")
}

/// Ordered `(name, text)` sections. Every section except the preamble is wrapped in `<name>` tags.
pub fn build_sections(options: &SystemPromptOptions<'_>) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = Vec::new();
    match options.custom_prompt {
        Some(prompt) => sections.push(("preamble".into(), prompt.into())),
        None => {
            sections.push(("preamble".into(), PREAMBLE.into()));
            let visible: Vec<String> = options
                .selected_tools
                .iter()
                .filter_map(|name| {
                    tool_info(name)
                        .filter(|info| !info.snippet.is_empty())
                        .map(|info| format!("- {name}: {}", info.snippet))
                })
                .collect();
            let tools = if visible.is_empty() { "(none)".to_string() } else { visible.join("\n") };
            sections.push((
                "tools".into(),
                format!("{tools}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project."),
            ));
            sections.push(("rules".into(), rules(options.selected_tools)));
            sections.push((
                "docs".into(),
                "Pi documentation (read only when the user asks about pi itself): this is pi-rust, a Rust port of the pi coding agent. See https://github.com/yannelli/pi-rust for usage, configuration, and supported providers.".into(),
            ));
        }
    }
    if let Some(append) = options.append_prompt.filter(|text| !text.trim().is_empty()) {
        sections.push(("addendum".into(), append.into()));
    }
    if !options.context_files.is_empty() {
        let files: Vec<String> = options
            .context_files
            .iter()
            .map(|file| {
                format!(
                    "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                    file.path.display(),
                    file.content
                )
            })
            .collect();
        sections.push((
            "project_context".into(),
            format!("Project-specific instructions and guidelines:\n\n{}", files.join("\n\n")),
        ));
    }
    sections.push(("cwd".into(), options.cwd.display().to_string().replace('\\', "/")));
    sections
        .into_iter()
        .map(
            |(name, text)| {
                if name == "preamble" { (name, text) } else { (name.clone(), format!("<{name}>\n{text}\n</{name}>")) }
            },
        )
        .collect()
}

pub fn build_system_prompt(options: &SystemPromptOptions<'_>) -> String {
    build_sections(options)
        .into_iter()
        .map(|(_, text)| text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn renders_tools_rules_context_and_cwd() {
        let tools: Vec<String> = ["read", "bash"].map(String::from).to_vec();
        let context = [ContextFile { path: PathBuf::from("/repo/AGENTS.md"), content: "Use tabs.".into() }];
        let prompt = build_system_prompt(&SystemPromptOptions {
            selected_tools: &tools,
            context_files: &context,
            append_prompt: Some("Extra."),
            ..SystemPromptOptions::new(Path::new("/repo"))
        });
        assert!(prompt.starts_with(PREAMBLE));
        assert!(prompt.contains("<tools>\n- read: "));
        assert!(prompt.contains("- Use bash for file operations like ls, rg, find"));
        assert!(prompt.contains("<addendum>\nExtra.\n</addendum>"));
        assert!(prompt.contains("<project_instructions path=\"/repo/AGENTS.md\">\nUse tabs.\n</project_instructions>"));
        assert!(prompt.ends_with("<cwd>\n/repo\n</cwd>"));
    }

    #[test]
    fn custom_prompt_replaces_default_sections() {
        let prompt = build_system_prompt(&SystemPromptOptions {
            custom_prompt: Some("Custom."),
            ..SystemPromptOptions::new(Path::new("/w"))
        });
        assert_eq!(prompt, "Custom.\n\n<cwd>\n/w\n</cwd>");
    }
}
