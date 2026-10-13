//! Slash command and `@` file completion for the editor (upstream `createBaseAutocompleteProvider`
//! and `core/slash-commands.ts`).

use std::rc::{Rc, Weak};

use pi_tui::autocomplete::CommandEntry;
use pi_tui::fuzzy::fuzzy_filter;
use pi_tui::{AutocompleteItem, CombinedAutocompleteProvider, SlashCommand};

use super::mode::InteractiveMode;
use crate::interactive_components::model_selector::model_search_text;
use crate::resources::source_info::{SourceInfo, SourceScope};

/// Built-in commands: name, description, and argument hint.
pub const BUILTIN_SLASH_COMMANDS: &[(&str, &str, Option<&str>)] = &[
    ("settings", "Open settings menu", None),
    ("model", "Select model (opens selector UI)", Some("<provider/model>")),
    ("tree", "Navigate session tree (switch branches)", None),
    ("thinking", "Set thinking level", Some("<level>")),
    ("scoped-models", "Enable/disable models for Ctrl+P cycling", None),
    ("export", "Export session (HTML default, or specify path: .html/.jsonl)", None),
    ("import", "Import and resume a session from a JSONL file", None),
    ("share", "Share session as a secret GitHub gist", None),
    ("bug", "Report a bug to the Pi developers", Some("<description>")),
    ("copy", "Copy last agent message to clipboard", None),
    ("name", "Set session display name", None),
    ("session", "Show session info and stats", None),
    ("changelog", "Show changelog entries", None),
    ("hotkeys", "Show all keyboard shortcuts", None),
    ("fork", "Create a new fork from a previous user message", None),
    ("clone", "Duplicate the current session at the current position", None),
    ("trust", "Save project trust decision for future sessions", None),
    ("login", "Configure provider authentication", Some("<provider>")),
    ("logout", "Remove provider authentication", None),
    ("new", "Start a new session", None),
    ("compact", "Manually compact the session context", None),
    ("resume", "Resume a different session", None),
    ("mcp", "Manage MCP servers: sign in, reconnect, enable or disable, and change exposure", None),
    ("llama", "Manage llama.cpp router models", None),
    ("reload", "Reload keybindings, extensions, skills, prompts, themes, and context files", None),
    ("quit", "Quit pi", None),
];

/// `[u]`, `[p:npm:pkg]`, or `[t:git:host/path@ref]` before a description; nothing for built-ins.
fn source_tag(source: &SourceInfo) -> Option<String> {
    if source.source == "builtin" {
        return None;
    }
    let scope = match source.scope {
        SourceScope::User => "u",
        SourceScope::Project => "p",
        SourceScope::Temporary => "t",
    };
    let origin = source.source.trim();
    if matches!(origin, "auto" | "local" | "cli") {
        return Some(scope.to_string());
    }
    if origin.starts_with("npm:") {
        return Some(format!("{scope}:{origin}"));
    }
    if let Some(git) = crate::packages::git_url::parse_git_url(origin) {
        let git_ref = git.git_ref.map(|value| format!("@{value}")).unwrap_or_default();
        return Some(format!("{scope}:git:{}/{}{git_ref}", git.host, git.path));
    }
    Some(scope.to_string())
}

fn prefixed_description(description: &str, source: &SourceInfo) -> Option<String> {
    match (source_tag(source), description.is_empty()) {
        (None, true) => None,
        (None, false) => Some(description.to_string()),
        (Some(tag), true) => Some(format!("[{tag}]")),
        (Some(tag), false) => Some(format!("[{tag}] {description}")),
    }
}

fn model_completions(weak: &Weak<InteractiveMode>, prefix: &str) -> Option<Vec<AutocompleteItem>> {
    let session = weak.upgrade()?.session();
    let scoped = session.scoped_models();
    let models = if scoped.is_empty() {
        session.model_runtime().available_models()
    } else {
        scoped.into_iter().map(|scoped| scoped.model).collect()
    };
    if models.is_empty() {
        return None;
    }
    let searchable: Vec<(String, pi_ai::Model)> = models.into_iter().map(|m| (model_search_text(&m), m)).collect();
    let filtered = fuzzy_filter(searchable, prefix, |(text, _)| text.as_str());
    (!filtered.is_empty()).then(|| {
        filtered
            .into_iter()
            .map(|(_, model)| {
                AutocompleteItem::new(format!("{}/{}", model.provider, model.id), model.id.clone())
                    .with_description(model.provider.clone())
            })
            .collect()
    })
}

fn thinking_completions(weak: &Weak<InteractiveMode>, prefix: &str) -> Option<Vec<AutocompleteItem>> {
    let levels: Vec<String> = weak
        .upgrade()?
        .session()
        .available_thinking_levels()
        .into_iter()
        .map(|level| level.as_str().to_string())
        .collect();
    let filtered = fuzzy_filter(levels, prefix, |level: &String| level.as_str());
    (!filtered.is_empty())
        .then(|| filtered.into_iter().map(|level| AutocompleteItem::new(level.clone(), level)).collect())
}

impl InteractiveMode {
    /// Commands, templates, and skills for the editor, plus `@` file search without `fd`.
    pub(super) fn setup_autocomplete_provider(&self) {
        let mut commands: Vec<CommandEntry> = Vec::new();
        for (name, description, hint) in BUILTIN_SLASH_COMMANDS {
            let mut command = SlashCommand::new(*name, Some(description));
            if let Some(hint) = hint {
                command = command.with_argument_hint(*hint);
            }
            let weak = self.weak.clone();
            command = match *name {
                "model" => command.with_argument_completions(move |prefix| model_completions(&weak, prefix)),
                "thinking" => command.with_argument_completions(move |prefix| thinking_completions(&weak, prefix)),
                "login" => command.with_argument_completions(move |prefix| {
                    weak.upgrade().and_then(|mode| mode.login_completions(prefix))
                }),
                "mcp" => command.with_argument_completions(move |prefix| {
                    weak.upgrade().and_then(|mode| mode.mcp_completions(prefix))
                }),
                _ => command,
            };
            commands.push(command.into());
        }
        let session = self.session();
        let services = session.services().clone();
        let resources = services.resources().resources().clone();
        for template in &resources.prompts {
            let mut command = SlashCommand::new(&template.name, None);
            command.description = prefixed_description(&template.description, &template.source_info);
            if let Some(hint) = &template.argument_hint {
                command = command.with_argument_hint(hint.clone());
            }
            commands.push(command.into());
        }
        let mut skills = std::collections::HashMap::new();
        if session.settings().enable_skill_commands() {
            for skill in &resources.skills {
                let name = format!("skill:{}", skill.name);
                skills.insert(name.clone(), skill.file_path.clone());
                let mut command = SlashCommand::new(name, None);
                command.description = prefixed_description(&skill.description, &skill.source_info);
                commands.push(command.into());
            }
        }
        self.state.borrow_mut().skill_commands = skills;
        let cwd = session.cwd().display().to_string();
        let provider = CombinedAutocompleteProvider::new(commands, cwd, None);
        self.editor.set_autocomplete_provider(Rc::new(provider));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::source_info::SourceOrigin;

    fn source(origin: &str, scope: SourceScope) -> SourceInfo {
        SourceInfo { path: String::new(), source: origin.into(), scope, origin: SourceOrigin::TopLevel, base_dir: None }
    }

    #[test]
    fn tags_descriptions_with_their_source() {
        assert_eq!(prefixed_description("Review", &source("builtin", SourceScope::User)).as_deref(), Some("Review"));
        assert_eq!(prefixed_description("Review", &source("local", SourceScope::User)).as_deref(), Some("[u] Review"));
        assert_eq!(
            prefixed_description("", &source("npm:pi-tools", SourceScope::Project)).as_deref(),
            Some("[p:npm:pi-tools]")
        );
        assert_eq!(
            prefixed_description("x", &source("git:github.com/a/b@v1", SourceScope::Temporary)).as_deref(),
            Some("[t:git:github.com/a/b@v1] x")
        );
    }
}
