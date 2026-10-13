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

impl Registry {
    pub fn command(&self, invocation_name: &str) -> Option<&CommandInfo> {
        self.commands.iter().find(|command| command.invocation_name == invocation_name)
    }

    pub fn flag(&self, name: &str) -> Option<&FlagInfo> {
        self.flags.iter().find(|flag| flag.name == name)
    }

    /// The `source` of an extension's source info (`local`, a package source, ...).
    pub fn source_of(&self, extension_path: &str) -> String {
        let info = self.extensions.iter().find(|extension| extension.path == extension_path);
        info.and_then(|extension| extension.source_info["source"].as_str()).unwrap_or("local").to_string()
    }
}
