//! How tools are offered: exposure, namespaces, annotations, and loadout changes. One
//! definition shared by built-in, extension, MCP, and codemode tools (upstream
//! `ToolExposure`, `ToolNamespace`, `ToolAnnotations`, `ToolLoadout`, `ToolLoadoutChanges`).

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::types::SharedTool;

/// How the model reaches a tool. "Callable" means callable from other tools, as codemode scripts
/// call them. The active tool set is the set declared to the model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolExposure {
    /// Declared to the model while active, and callable while active.
    #[default]
    Direct,
    /// Declared to the model while active, never callable. For orchestrating or interactive tools.
    ModelOnly,
    /// Callable whenever registered; declared only when activated explicitly. Codemode lists it.
    Codemode,
    /// Like `Codemode`, but codemode does not list it; tool search can find it.
    Deferred,
    /// Registered but unreachable. Activating it has no effect.
    Hidden,
}

impl ToolExposure {
    /// Whether activating the tool declares it to the model.
    pub fn is_declarable(self) -> bool {
        matches!(self, Self::Direct | Self::ModelOnly)
    }

    /// Whether other tools can call it while it is not active.
    pub fn is_callable_while_inactive(self) -> bool {
        matches!(self, Self::Codemode | Self::Deferred)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::ModelOnly => "model-only",
            Self::Codemode => "codemode",
            Self::Deferred => "deferred",
            Self::Hidden => "hidden",
        }
    }
}

/// Hints with the meaning of MCP tool annotations. They come from the tool's author, unverified;
/// permission checks can use them to decide which calls to confirm.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    /// The tool leaves its environment unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// The tool may delete or overwrite data. Meaningful when not read-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// Repeating a call with the same arguments has no further effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// The tool reaches an open world of external entities, such as the web.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// A group of related tools, such as the tools of one MCP server (`mcp__docs`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolNamespace {
    pub name: String,
    /// Short summary shown once with the group in tool listings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Longer guidance, like server instructions. Not part of listings; returned on request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl ToolNamespace {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), ..Self::default() }
    }
}

/// Recovery policy for an effect whose durable intent exists but whose outcome is unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolReplay {
    Never,
    Safe,
}

/// The tools of a session as `prepare_loadout` sees them.
#[derive(Clone, Default)]
pub struct ToolLoadout {
    /// Tools declared to the model (the active tools), in order, with their original descriptions.
    pub declared: Vec<SharedTool>,
    /// Tools callable from other tools.
    pub callable: Vec<SharedTool>,
    /// Every registered tool.
    pub registered: Vec<SharedTool>,
    /// Normalized prompt guidelines by tool name.
    pub guidelines: HashMap<String, Vec<String>>,
}

impl ToolLoadout {
    fn find(&self, name: &str) -> Option<&SharedTool> {
        self.registered.iter().chain(&self.declared).chain(&self.callable).find(|tool| tool.name() == name)
    }

    /// A tool's exposure; unknown tools are `Direct`.
    pub fn exposure(&self, name: &str) -> ToolExposure {
        self.find(name).map(|tool| tool.exposure()).unwrap_or_default()
    }

    pub fn namespace(&self, name: &str) -> Option<ToolNamespace> {
        self.find(name).and_then(|tool| tool.namespace().cloned())
    }

    /// A tool's prompt guidelines. Hidden declarations leave them out of the system prompt.
    pub fn prompt_guidelines(&self, name: &str) -> Vec<String> {
        match self.guidelines.get(name) {
            Some(guidelines) => guidelines.clone(),
            None => {
                self.find(name).map(|tool| normalize_prompt_guidelines(&tool.prompt_guidelines())).unwrap_or_default()
            }
        }
    }
}

/// Changes `prepare_loadout` makes to what the model sees.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolLoadoutChanges {
    /// Model-facing descriptions of declared tools, by tool name.
    pub descriptions: BTreeMap<String, String>,
    /// Declared tools whose declarations requests leave out. They stay active and callable, and
    /// the transcript still declares them, so the active set survives tree navigation and resume.
    pub hidden_declarations: Vec<String>,
}

/// A prompt snippet on one line, or `None` when empty.
pub fn normalize_prompt_snippet(text: Option<&str>) -> Option<String> {
    let line = text?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!line.is_empty()).then_some(line)
}

/// Trimmed, non-empty, unique guidelines in their original order.
pub fn normalize_prompt_guidelines(guidelines: &[String]) -> Vec<String> {
    let mut unique: Vec<String> = Vec::new();
    for guideline in guidelines.iter().map(|guideline| guideline.trim()).filter(|guideline| !guideline.is_empty()) {
        if !unique.iter().any(|existing| existing == guideline) {
            unique.push(guideline.to_string());
        }
    }
    unique
}
