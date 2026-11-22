//! Module namespaces: each one has its own instances of ES and CommonJS modules. A module in a
//! namespace has the key `pi-ns:<id>:<key>`, and what it imports inherits the namespace.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLock};

use rquickjs::{Ctx, Module, Promise};

use super::resolve_in;
use crate::error::JsError;
use crate::resolve::{ResolveKind, canonical, normalize};
use crate::runtime::state::{Shared, state};
use crate::virtual_modules::{VirtualModule, VirtualModules};

const PREFIX: &str = "pi-ns:";
/// The file URL query that shows a module's namespace in `import.meta.url`: `file:///a.js?pi-ns=<id>`.
pub(crate) const QUERY: &str = "pi-ns";

/// Registered namespaces by id.
pub(crate) type Namespaces = RwLock<HashMap<String, Arc<ModuleNamespace>>>;

/// A set of module instances of its own. Built-in modules, `pi:host`, and the keys passed to
/// [`ModuleNamespace::share`] keep one instance for the whole runtime.
#[derive(Clone, Debug)]
pub struct ModuleNamespace {
    id: String,
    shared: HashSet<String>,
    virtual_modules: VirtualModules,
}

impl ModuleNamespace {
    /// `id` is 1 to 64 ASCII letters, digits, `-`, or `_`.
    pub fn new(id: impl Into<String>) -> Result<Self, JsError> {
        let id = id.into();
        let valid =
            (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !valid {
            return Err(namespace_error(format!("Invalid module namespace id {id:?}")));
        }
        Ok(Self { id, shared: HashSet::new(), virtual_modules: VirtualModules::new() })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Give every namespace the runtime's one instance of `key`: an absolute path, or the
    /// canonical name of a virtual module.
    pub fn share(&mut self, key: impl AsRef<str>) -> &mut Self {
        let key = key.as_ref();
        let path = Path::new(key);
        if path.is_absolute() {
            let normal = normalize(path);
            self.shared.insert(canonical(&normal).display().to_string());
            self.shared.insert(normal.display().to_string());
        } else {
            self.shared.insert(key.to_string());
        }
        self
    }

    /// A virtual module only this namespace resolves. It takes precedence over a runtime-wide
    /// virtual module of the same name.
    pub fn insert_virtual_module(&mut self, name: impl Into<String>, module: VirtualModule) -> &mut Self {
        self.virtual_modules.insert(name, module);
        self
    }

    /// Whether the module `key` gets an instance of this namespace's own.
    pub(crate) fn owns(&self, key: &str) -> bool {
        self.virtual_modules.get(key).is_some() || !self.shared.contains(key)
    }
}

fn namespace_error(message: String) -> JsError {
    JsError::new(message).with_code("ERR_PI_MODULE_NAMESPACE")
}

pub(crate) fn hooks_unsupported() -> JsError {
    JsError::new("module.register hooks cannot load modules in a module namespace in pi-rust")
        .with_code(crate::UNSUPPORTED_CODE)
}

pub(crate) fn register(shared: &Shared, namespace: ModuleNamespace) -> Result<(), JsError> {
    let mut namespaces = shared.namespaces.write().unwrap_or_else(|poison| poison.into_inner());
    if namespaces.contains_key(&namespace.id) {
        return Err(namespace_error(format!("Module namespace {:?} is already registered", namespace.id)));
    }
    namespaces.insert(namespace.id.clone(), Arc::new(namespace));
    Ok(())
}

pub(crate) fn lookup(shared: &Shared, id: &str) -> Result<Arc<ModuleNamespace>, JsError> {
    let namespaces = shared.namespaces.read().unwrap_or_else(|poison| poison.into_inner());
    namespaces.get(id).cloned().ok_or_else(|| namespace_error(format!("Unknown module namespace {id:?}")))
}

/// The key of the module `key` in the namespace `id`.
pub(crate) fn key(id: &str, key: &str) -> String {
    format!("{PREFIX}{id}:{key}")
}

/// The namespace and the plain key of a module key; no namespace for a plain key.
pub(crate) fn split(key: &str) -> (Option<&str>, &str) {
    match key.strip_prefix(PREFIX).and_then(|rest| rest.split_once(':')) {
        Some((id, plain)) => (Some(id), plain),
        None => (None, key),
    }
}

/// `url` with the query that names the namespace `id`.
pub(crate) fn url_in(url: &str, id: &str) -> String {
    format!("{url}?{QUERY}={id}")
}

/// A virtual module, looked up in the namespace `id` first.
pub(crate) fn virtual_module(shared: &Shared, id: Option<&str>, name: &str) -> Result<Option<VirtualModule>, JsError> {
    if let Some(id) = id
        && let Some(module) = lookup(shared, id)?.virtual_modules.get(name)
    {
        return Ok(Some(module.clone()));
    }
    Ok(shared.virtual_modules.read().unwrap_or_else(|poison| poison.into_inner()).get(name).cloned())
}

/// A resolved module: its kind, its plain key, and its namespace (`None` for the shared instance).
pub(crate) struct Scoped {
    pub kind: &'static str,
    pub key: String,
    pub namespace: Option<String>,
}

impl Scoped {
    pub fn module_key(&self) -> String {
        match &self.namespace {
            Some(id) => key(id, &self.key),
            None => self.key.clone(),
        }
    }
}

/// [`resolve_in`] for an importer in the namespace `namespace`. Specifiers never name a
/// namespace: a result that looks like a namespaced key is not found.
pub(crate) fn resolve_scoped(
    shared: &Shared,
    base_dir: &Path,
    inline: &dyn Fn(&str) -> bool,
    specifier: &str,
    from: Option<&str>,
    kind: ResolveKind,
    namespace: Option<&str>,
) -> Result<Scoped, JsError> {
    let namespace = namespace.map(|id| lookup(shared, id)).transpose()?;
    if let Some(namespace) = &namespace
        && namespace.virtual_modules.get(specifier).is_some()
    {
        return Ok(Scoped { kind: "virtual", key: specifier.to_string(), namespace: Some(namespace.id.clone()) });
    }
    let (found, key) = resolve_in(shared, base_dir, inline, specifier, from, kind)?;
    if key.starts_with(PREFIX) {
        let code = if kind == ResolveKind::Import { "ERR_MODULE_NOT_FOUND" } else { "MODULE_NOT_FOUND" };
        return Err(JsError::new(format!("Cannot find module '{specifier}'")).with_code(code));
    }
    let namespace = namespace.filter(|namespace| found != "builtin" && namespace.owns(&key));
    Ok(Scoped { kind: found, key, namespace: namespace.map(|namespace| namespace.id.clone()) })
}

/// Import the module `key` from Rust. The resolver returns a namespaced key as is only when it
/// is the specifier of this import.
pub(crate) fn import<'js>(ctx: &Ctx<'js>, key: &str) -> rquickjs::Result<Promise<'js>> {
    if split(key).0.is_none() {
        return Module::import(ctx, key);
    }
    let state = state();
    *state.trusted_import.borrow_mut() = Some(key.to_string());
    let result = Module::import(ctx, key);
    state.trusted_import.borrow_mut().take();
    result
}

/// Whether `name` is the key an [`import`] in progress named; it is accepted once.
pub(crate) fn take_trusted(name: &str) -> bool {
    let state = state();
    let mut trusted = state.trusted_import.borrow_mut();
    if trusted.as_deref() == Some(name) {
        trusted.take();
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_split_back_into_namespace_and_plain_key() {
        for plain in ["/a/b.js", "/a/q?x.mjs", r"C:\a\b.js", "pi:runner", "@earendil-works/pi-tui"] {
            assert_eq!(split(&key("ns-1", plain)), (Some("ns-1"), plain));
            assert_eq!(split(plain), (None, plain));
        }
        assert_eq!(url_in("file:///a/q%3Fx.mjs", "a"), "file:///a/q%3Fx.mjs?pi-ns=a");
    }

    #[test]
    fn shared_keys_and_own_virtual_modules() {
        let base = std::env::temp_dir();
        let mut namespace = ModuleNamespace::new("a").unwrap();
        namespace.share(base.join("x").join("..").join("y.js").display().to_string()).share("typebox");
        namespace.insert_virtual_module("pi:runner", VirtualModule::Json(serde_json::json!({})));
        assert!(!namespace.owns(&normalize(&base.join("y.js")).display().to_string()));
        assert!(!namespace.owns("typebox"));
        assert!(namespace.owns("pi:runner"));
        assert!(namespace.owns(&base.join("z.js").display().to_string()));
    }
}
