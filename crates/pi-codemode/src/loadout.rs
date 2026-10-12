//! How the `codemode` tool presents the other tools while it is active.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::description::{
    CODEMODE_TOOL_NAME, CodemodeDescriptionOptions, CodemodeMode, DEFAULT_CODEMODE_INLINE_BUDGET,
    codemode_callable_tools, create_codemode_description, describe_script_call,
};
use crate::host::{CodemodeToolInfo, ToolExposure};

/// File name of the codemode reference (globals, tool results, `store()`, the `models` API).
pub const CODEMODE_DOCS_FILE_NAME: &str = "codemode.md";

/// Settings of the `codemode` tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodemodeToolOptions {
    /// The `codemode.mode` setting.
    pub mode: CodemodeMode,
    /// Estimated tokens for tool declarations in the description (`codemode.inlineBudget`).
    pub inline_budget: usize,
    /// Expose the host's model registry to scripts as `models`.
    pub models: bool,
    /// Path of the codemode reference that the description points to for the `models` API.
    pub docs_path: String,
}

impl Default for CodemodeToolOptions {
    fn default() -> Self {
        Self {
            mode: CodemodeMode::On,
            inline_budget: DEFAULT_CODEMODE_INLINE_BUDGET,
            models: true,
            docs_path: CODEMODE_DOCS_FILE_NAME.into(),
        }
    }
}

/// The tools of a session as the `codemode` tool sees them when a request is prepared.
#[derive(Clone, Debug, Default)]
pub struct ToolLoadout {
    /// Tools declared to the model (the active tools), in order, with their original descriptions.
    pub declared: Vec<CodemodeToolInfo>,
    /// Tools callable through the host.
    pub callable: Vec<CodemodeToolInfo>,
    /// Exposure by tool name. Missing names are `direct`.
    pub exposures: HashMap<String, ToolExposure>,
}

impl ToolLoadout {
    pub fn exposure(&self, name: &str) -> ToolExposure {
        self.exposures.get(name).copied().unwrap_or_default()
    }
}

/// Changes the `codemode` tool makes to what the model sees.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolLoadoutChanges {
    /// Model-facing descriptions of declared tools, by tool name.
    pub descriptions: BTreeMap<String, String>,
    /// Declared tools whose declarations requests leave out. They stay active and callable.
    pub hidden_declarations: Vec<String>,
}

/// `on`: declared tools that scripts can call say how; the description lists callable tools that
/// are not `direct`. `only`: the description lists every callable tool, and the declarations of
/// active `direct` tools are hidden. Listing by exposure keeps the description stable across loads.
pub fn prepare_codemode_loadout(loadout: &ToolLoadout, options: &CodemodeToolOptions) -> ToolLoadoutChanges {
    let is_direct = |tool: &CodemodeToolInfo| loadout.exposure(&tool.name) == ToolExposure::Direct;
    let callable = codemode_callable_tools(&loadout.callable);
    let callable_names: HashSet<&str> = callable.iter().map(|tool| tool.name.as_str()).collect();
    let mut descriptions = BTreeMap::new();
    if options.mode == CodemodeMode::On {
        for tool in loadout.declared.iter().filter(|tool| callable_names.contains(tool.name.as_str())) {
            descriptions.insert(tool.name.clone(), describe_script_call(tool));
        }
    }
    let listed: Vec<CodemodeToolInfo> = match options.mode {
        CodemodeMode::Only => callable.clone(),
        CodemodeMode::On => callable.iter().filter(|tool| !is_direct(tool)).cloned().collect(),
    };
    let description = create_codemode_description(
        &listed,
        &CodemodeDescriptionOptions {
            models_docs_path: options.models.then(|| options.docs_path.clone()),
            deferred: listed
                .iter()
                .filter(|tool| loadout.exposure(&tool.name) == ToolExposure::Deferred)
                .map(|tool| tool.name.clone())
                .collect(),
            inline_budget: Some(options.inline_budget),
        },
    );
    descriptions.insert(CODEMODE_TOOL_NAME.into(), description);
    let declared: HashSet<&str> = loadout.declared.iter().map(|tool| tool.name.as_str()).collect();
    let hidden_declarations = match options.mode {
        CodemodeMode::Only => callable
            .iter()
            .filter(|tool| is_direct(tool) && declared.contains(tool.name.as_str()))
            .map(|tool| tool.name.clone())
            .collect(),
        CodemodeMode::On => Vec::new(),
    };
    ToolLoadoutChanges { descriptions, hidden_declarations }
}
