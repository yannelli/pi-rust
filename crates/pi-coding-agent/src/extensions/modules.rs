//! Modules extensions import (upstream `virtual-modules.ts`): typebox and pi-tui as embedded
//! upstream code, pi-ai (compat and models), pi-agent-core, and pi-coding-agent as host modules
//! over the session. Every alias of upstream's table resolves to one shared instance.

use std::path::PathBuf;

use pi_js::{VirtualModule, VirtualModules};

use super::vendor::{self, vendor_root};

/// The host modules, as files under `<vendor root>/host`.
const HOST_FILES: &[(&str, &str)] = &[
    ("agent_core.js", include_str!("js/agent_core.js")),
    ("unsupported.js", include_str!("js/unsupported.js")),
    ("ai.js", include_str!("js/ai.js")),
    ("ai_extras.js", include_str!("js/ai_extras.js")),
    ("api.js", include_str!("js/api.js")),
    ("bridge.js", include_str!("js/bridge.js")),
    ("coding_agent.js", include_str!("js/coding_agent.js")),
    ("coding_utils.js", include_str!("js/coding_utils.js")),
    ("coding_unsupported.js", include_str!("js/coding_unsupported.js")),
    ("compaction_utils.js", include_str!("js/compaction_utils.js")),
    ("context.js", include_str!("js/context.js")),
    ("host.js", include_str!("js/host.js")),
    ("models.js", include_str!("js/models.js")),
    ("runner.js", include_str!("js/runner.js")),
    ("sdk_session.js", include_str!("js/sdk_session.js")),
    ("sdk_storage.js", include_str!("js/sdk_storage.js")),
    ("sdk_resources.js", include_str!("js/sdk_resources.js")),
    ("session_utils.js", include_str!("js/session_utils.js")),
    ("stream.js", include_str!("js/stream.js")),
    ("theme.js", include_str!("js/theme.js")),
    ("ui.js", include_str!("js/ui.js")),
];

/// Upstream components import their theme from here; it is the host theme module.
const THEME_SHIM: &str = "pi-coding-agent/src/modes/interactive/theme/theme.ts";

fn path(key: &str) -> String {
    vendor::specifier(&vendor_root(), key)
}

fn reexport(key: &str) -> VirtualModule {
    VirtualModule::Source(format!("export * from \"{}\";\n", path(key)))
}

/// The module Rust imports to create the host object.
pub(crate) fn host_specifier() -> String {
    path("host/host.js")
}

/// Embedded upstream files and the host modules, keyed by absolute path.
pub(crate) fn source_files() -> Vec<(PathBuf, String)> {
    let root = vendor_root();
    let mut files = vendor::source_files(&root);
    files.extend(HOST_FILES.iter().map(|(name, source)| (root.join("host").join(name), source.to_string())));
    files.push((root.join(THEME_SHIM), format!("export * from \"{}\";\n", path("host/theme.js"))));
    files
}

/// Modules for the canonical names; [`pi_js::VIRTUAL_MODULE_ALIASES`] maps the others to these.
pub(crate) fn virtual_modules() -> VirtualModules {
    let mut modules = VirtualModules::new();
    modules.insert("typebox", reexport("typebox/build/index.mjs"));
    modules.insert("typebox/compile", reexport("typebox/build/compile/index.mjs"));
    modules.insert("typebox/value", reexport("typebox/build/value/index.mjs"));
    modules.insert("@earendil-works/pi-tui", reexport("pi-tui/src/index.ts"));
    modules.insert("@earendil-works/pi-ai/compat", reexport("host/ai.js"));
    modules.insert("@earendil-works/pi-ai/models", reexport("host/models.js"));
    modules.insert("@earendil-works/pi-ai/oauth", VirtualModule::Json(serde_json::json!({})));
    modules.insert(
        "@earendil-works/pi-ai/providers/all",
        VirtualModule::Source(format!(
            "export {{ getBuiltinModel, getBuiltinModels, getBuiltinProviders }} from \"{}\";\n",
            path("host/ai.js")
        )),
    );
    modules.insert("@earendil-works/pi-agent-core", reexport("host/agent_core.js"));
    modules.insert("@earendil-works/pi-coding-agent", reexport("host/coding_agent.js"));
    modules
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_upstream_virtual_module_is_registered() {
        let modules = virtual_modules();
        for name in pi_js::PI_VIRTUAL_MODULES {
            assert!(modules.get(name).is_some(), "{name}");
        }
        for (alias, _) in pi_js::VIRTUAL_MODULE_ALIASES {
            assert!(modules.canonical_name(alias).is_some(), "{alias}");
        }
    }
}
