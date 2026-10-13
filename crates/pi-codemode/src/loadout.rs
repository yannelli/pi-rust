//! How the `codemode` tool presents the other tools while it is active.

use std::collections::{BTreeMap, HashSet};

use pi_agent::{SharedTool, ToolExposure};
pub use pi_agent::{ToolLoadout, ToolLoadoutChanges};

use crate::description::{
    CODEMODE_TOOL_NAME, CodemodeDescriptionOptions, CodemodeMode, DEFAULT_CODEMODE_INLINE_BUDGET,
    codemode_callable_tools, create_codemode_description, describe_script_call,
};
use crate::host::CodemodeToolInfo;

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

/// `on`: declared tools that scripts can call say how; the description lists callable tools that
/// are not `direct`. `only`: the description lists every callable tool, and the declarations of
/// active `direct` tools are hidden. Listing by exposure keeps the description stable across loads.
pub fn prepare_codemode_loadout(loadout: &ToolLoadout, options: &CodemodeToolOptions) -> ToolLoadoutChanges {
    let is_direct = |tool: &CodemodeToolInfo| loadout.exposure(&tool.name) == ToolExposure::Direct;
    // Listings show each tool with the guidelines the session normalized for it.
    let info = |tool: &SharedTool| CodemodeToolInfo {
        prompt_guidelines: loadout.prompt_guidelines(tool.name()),
        namespace: loadout.namespace(tool.name()),
        ..CodemodeToolInfo::from_tool(tool.as_ref())
    };
    let declared_infos: Vec<CodemodeToolInfo> = loadout.declared.iter().map(info).collect();
    let callable_infos: Vec<CodemodeToolInfo> = loadout.callable.iter().map(info).collect();
    let callable = codemode_callable_tools(&callable_infos);
    let callable_names: HashSet<&str> = callable.iter().map(|tool| tool.name.as_str()).collect();
    let mut descriptions = BTreeMap::new();
    if options.mode == CodemodeMode::On {
        for tool in declared_infos.iter().filter(|tool| callable_names.contains(tool.name.as_str())) {
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
    let declared: HashSet<&str> = declared_infos.iter().map(|tool| tool.name.as_str()).collect();
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
