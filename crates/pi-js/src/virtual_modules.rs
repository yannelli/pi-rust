//! Modules registered from Rust, and the specifier aliases upstream pi gives extensions.

use std::collections::HashMap;
use std::sync::Arc;

use rquickjs::Object;

use crate::error::JsError;
use crate::runtime::JsScope;

/// Canonical names of the modules the extension host registers (upstream `VIRTUAL_MODULES`).
pub const PI_VIRTUAL_MODULES: &[&str] = &[
    "typebox",
    "typebox/compile",
    "typebox/value",
    "@earendil-works/pi-agent-core",
    "@earendil-works/pi-tui",
    "@earendil-works/pi-ai/compat",
    "@earendil-works/pi-ai/models",
    "@earendil-works/pi-ai/oauth",
    "@earendil-works/pi-ai/providers/all",
    "@earendil-works/pi-coding-agent",
];

/// `(alias, canonical)` pairs. An alias shares the module instance of its canonical name.
/// The pi-ai root resolves to the compat entry point, as upstream does.
pub const VIRTUAL_MODULE_ALIASES: &[(&str, &str)] = &[
    ("@sinclair/typebox", "typebox"),
    ("@sinclair/typebox/compile", "typebox/compile"),
    ("@sinclair/typebox/value", "typebox/value"),
    ("@earendil-works/pi-ai", "@earendil-works/pi-ai/compat"),
    ("@mariozechner/pi-agent-core", "@earendil-works/pi-agent-core"),
    ("@mariozechner/pi-tui", "@earendil-works/pi-tui"),
    ("@mariozechner/pi-ai", "@earendil-works/pi-ai/compat"),
    ("@mariozechner/pi-ai/compat", "@earendil-works/pi-ai/compat"),
    ("@mariozechner/pi-ai/models", "@earendil-works/pi-ai/models"),
    ("@mariozechner/pi-ai/oauth", "@earendil-works/pi-ai/oauth"),
    ("@mariozechner/pi-ai/providers/all", "@earendil-works/pi-ai/providers/all"),
    ("@mariozechner/pi-coding-agent", "@earendil-works/pi-coding-agent"),
];

/// Builds a native module's exports on the JavaScript thread, once per runtime.
pub type NativeModuleFn = dyn for<'js> Fn(&JsScope<'js>) -> Result<Object<'js>, JsError> + Send + Sync;

/// How a virtual module provides its exports.
#[derive(Clone)]
pub enum VirtualModule {
    /// ES module source text. It may import `pi:host` and other modules.
    Source(String),
    /// A JSON value: the default export, plus a named export per top-level key.
    Json(serde_json::Value),
    /// An exports object built in Rust: the default export, plus a named export per key.
    Native(Arc<NativeModuleFn>),
}

impl VirtualModule {
    pub fn native(
        build: impl for<'js> Fn(&JsScope<'js>) -> Result<Object<'js>, JsError> + Send + Sync + 'static,
    ) -> Self {
        Self::Native(Arc::new(build))
    }
}

impl std::fmt::Debug for VirtualModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(source) => f.debug_tuple("Source").field(&source.len()).finish(),
            Self::Json(value) => f.debug_tuple("Json").field(value).finish(),
            Self::Native(_) => f.write_str("Native"),
        }
    }
}

/// Registered virtual modules by name.
#[derive(Clone, Debug, Default)]
pub struct VirtualModules {
    modules: HashMap<String, VirtualModule>,
}

impl VirtualModules {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl Into<String>, module: VirtualModule) -> &mut Self {
        self.modules.insert(name.into(), module);
        self
    }

    pub fn get(&self, name: &str) -> Option<&VirtualModule> {
        self.modules.get(name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.modules.keys().cloned().collect();
        names.sort();
        names
    }

    /// The registered name `specifier` loads, following the alias table both ways, so
    /// `@mariozechner/pi-tui` finds a module registered as `@earendil-works/pi-tui`.
    pub fn canonical_name(&self, specifier: &str) -> Option<String> {
        if self.modules.contains_key(specifier) {
            return Some(specifier.to_string());
        }
        let target = alias_target(specifier).unwrap_or(specifier);
        std::iter::once(target)
            .chain(VIRTUAL_MODULE_ALIASES.iter().filter(|(_, to)| *to == target).map(|(from, _)| *from))
            .find(|name| self.modules.contains_key(*name))
            .map(str::to_string)
    }
}

/// The canonical name an alias points to.
pub fn alias_target(specifier: &str) -> Option<&'static str> {
    VIRTUAL_MODULE_ALIASES.iter().find(|(from, _)| *from == specifier).map(|(_, to)| *to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_share_one_registration_in_either_direction() {
        let mut modules = VirtualModules::new();
        modules.insert("@earendil-works/pi-tui", VirtualModule::Json(serde_json::json!({})));
        modules.insert("@earendil-works/pi-ai", VirtualModule::Json(serde_json::json!({})));
        assert_eq!(modules.canonical_name("@mariozechner/pi-tui").as_deref(), Some("@earendil-works/pi-tui"));
        assert_eq!(modules.canonical_name("@earendil-works/pi-ai/compat").as_deref(), Some("@earendil-works/pi-ai"));
        assert_eq!(modules.canonical_name("@mariozechner/pi-ai").as_deref(), Some("@earendil-works/pi-ai"));
        assert_eq!(modules.canonical_name("typebox"), None);
        assert_eq!(alias_target("@sinclair/typebox/value"), Some("typebox/value"));
        for (alias, target) in VIRTUAL_MODULE_ALIASES {
            assert!(PI_VIRTUAL_MODULES.contains(target), "{alias} -> {target}");
        }
    }
}
