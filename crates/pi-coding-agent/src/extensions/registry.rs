//! What the loaded extensions registered, as JavaScript publishes it after every change: event
//! types with handlers, tools, commands, flags, shortcuts, and renderers.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::Value;

/// A tool an extension registered (upstream `ToolDefinition` without its functions).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ToolDescriptor {
    pub name: String,
    pub label: String,
    pub description: String,
    pub parameters: Value,
    pub prompt_snippet: Option<String>,
    pub prompt_guidelines: Option<Vec<String>>,
    pub constrained_sampling: Option<Value>,
    pub render_shell: Option<String>,
    pub output_schema: Option<Value>,
    pub exposure: Option<String>,
    pub namespace: Option<Value>,
    pub annotations: Option<Value>,
    pub default_active: Option<bool>,
    pub execution_mode: Option<String>,
    pub has_prepare_arguments: bool,
    pub has_prepare_loadout: bool,
    pub has_render_call: bool,
    pub has_render_result: bool,
    pub source_info: Value,
    pub extension_path: String,
}

/// A registered slash command; `invocation_name` is `name` or `name:<n>` for duplicates.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct CommandInfo {
    pub name: String,
    pub invocation_name: String,
    pub description: Option<String>,
    pub source_info: Value,
    pub has_completions: bool,
}

/// A CLI flag (`--name`); `default` is the current value.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct FlagInfo {
    pub name: String,
    pub description: Option<String>,
    /// `boolean` or `string`.
    #[serde(rename = "type")]
    pub kind: String,
    pub default: Option<Value>,
    pub extension_path: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ShortcutInfo {
    pub shortcut: String,
    pub description: Option<String>,
    pub extension_path: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct LoadedExtension {
    pub path: String,
    pub source_info: Value,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Registry {
    pub handlers: HashSet<String>,
    pub tools: Vec<ToolDescriptor>,
    pub commands: Vec<CommandInfo>,
    pub flags: Vec<FlagInfo>,
    pub shortcuts: Vec<ShortcutInfo>,
    pub message_renderers: Vec<String>,
    pub entry_renderers: Vec<String>,
    pub tool_renderers: usize,
    pub markdown_transformers: usize,
    pub extensions: Vec<LoadedExtension>,
}

/// An extension registration that keeps a built-in extension from loading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltinReplacement {
    /// `tool` or `command`.
    pub kind: &'static str,
    /// `/mcp` for a command, the tool name for a tool.
    pub registered: String,
    pub extension_path: String,
}

impl BuiltinReplacement {
    /// Upstream's warning for the built-in extension `builtin` left out.
    pub fn warning(&self, builtin: &str) -> String {
        format!(
            "Extension {} registers {} `{}`, so built-in extension `{builtin}` was not loaded. To use `{builtin}`, run \
             `pi config` and make sure it is enabled under Built-in extensions, then disable or remove the existing \
             extension. We recommend only having one or the other loaded at a time.",
            self.extension_path, self.kind, self.registered
        )
    }
}

impl Registry {
    pub fn command(&self, invocation_name: &str) -> Option<&CommandInfo> {
        self.commands.iter().find(|command| command.invocation_name == invocation_name)
    }

    pub fn flag(&self, name: &str) -> Option<&FlagInfo> {
        self.flags.iter().find(|flag| flag.name == name)
    }

    /// The tool or command an extension registered that replaces built-in extension `builtin`
    /// (`mcp`, `codemode`, `tool-search`), as upstream `omitReplacedExtensions` finds it.
    pub fn builtin_replacement(&self, builtin: &str) -> Option<BuiltinReplacement> {
        let (kind, name) = match builtin {
            "mcp" => ("command", "mcp"),
            "codemode" => ("tool", "codemode"),
            "tool-search" => ("tool", "tool_search"),
            _ => return None,
        };
        let extension_path = match kind {
            "command" => self
                .commands
                .iter()
                .find(|command| command.name == name)
                .map(|command| command.source_info["path"].as_str().unwrap_or_default().to_string()),
            _ => self.tools.iter().find(|tool| tool.name == name).map(|tool| tool.extension_path.clone()),
        }?;
        let registered = if kind == "command" { format!("/{name}") } else { name.to_string() };
        Some(BuiltinReplacement { kind, registered, extension_path })
    }

    /// The `source` of an extension's source info (`local`, a package source, ...).
    pub fn source_of(&self, extension_path: &str) -> String {
        let info = self.extensions.iter().find(|extension| extension.path == extension_path);
        info.and_then(|extension| extension.source_info["source"].as_str()).unwrap_or("local").to_string()
    }
}
