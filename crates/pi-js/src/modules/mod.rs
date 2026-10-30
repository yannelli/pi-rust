//! The QuickJS module resolver and loader. ES modules load natively; CommonJS, JSON, built-in,
//! and value-backed virtual modules load through facades that re-export an object's keys.

pub(crate) mod cjs;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::{Ctx, Function, Module, Runtime, Value};

use crate::error::JsError;
use crate::resolve::{ResolveKind, Resolved, normalize};
use crate::runtime::convert::{caught, throw};
use crate::runtime::state::{LoopState, Shared, state};
use crate::transpile::{FreeGlobals, ModuleFormat, register_source_map};
use crate::virtual_modules::VirtualModule;

pub(crate) fn install_loader(runtime: &Runtime) {
    runtime.set_loader(PiResolver, PiLoader);
}

/// Resolve to the module key QuickJS caches modules under: an absolute path, `node:x`,
/// `pi:host`, or a virtual module's canonical name.
pub(crate) fn resolve_key(specifier: &str, from: Option<&str>) -> Result<String, JsError> {
    resolve_kind(specifier, from, ResolveKind::Import).map(|(_, key)| key)
}

/// Like [`resolve_key`], also naming the kind: `builtin`, `virtual`, or `file`.
pub(crate) fn resolve_kind(
    specifier: &str,
    from: Option<&str>,
    kind: ResolveKind,
) -> Result<(&'static str, String), JsError> {
    let state = state();
    let base_dir = state.cwd.borrow().clone();
    let inline = |key: &str| state.inline_sources.borrow().contains_key(key);
    resolve_in(&state.shared, &base_dir, &inline, specifier, from, kind)
}

/// [`resolve_kind`] for any backend: `inline` tells whether a key names a module evaluated from
/// a string.
pub(crate) fn resolve_in(
    shared: &Shared,
    base_dir: &Path,
    inline: &dyn Fn(&str) -> bool,
    specifier: &str,
    from: Option<&str>,
    kind: ResolveKind,
) -> Result<(&'static str, String), JsError> {
    let from_path = from.map(Path::new).filter(|path| path.is_absolute());
    let has_source = |key: &str| inline(key) || registered_source(shared, key).is_some();
    if has_source(specifier) {
        return Ok(("file", specifier.to_string()));
    }
    // Absolute specifiers too: on Windows `C:/x/y.js` and `C:\x/y.js` name the key `C:\x\y.js`.
    let candidate = if specifier.starts_with('.') {
        let dir = from_path.and_then(Path::parent).unwrap_or(base_dir);
        Some(normalize(&dir.join(specifier)).display().to_string())
    } else if Path::new(specifier).is_absolute() {
        Some(normalize(Path::new(specifier)).display().to_string())
    } else {
        None
    };
    if let Some(candidate) = candidate {
        if has_source(&candidate) {
            return Ok(("file", candidate));
        }
        if let Some(found) = probe_source_file(shared, &candidate) {
            return Ok(("file", found));
        }
    }
    let virtual_modules = shared.virtual_modules.read().unwrap_or_else(|poison| poison.into_inner());
    let lookup = |name: &str| virtual_modules.canonical_name(name);
    let resolved = shared.resolver.resolve(specifier, from_path, base_dir, kind, &lookup)?;
    Ok(match resolved {
        Resolved::Builtin(name) => ("builtin", name),
        Resolved::Virtual(name) => ("virtual", name),
        Resolved::File(path) => ("file", path.display().to_string()),
    })
}

/// Whether `key` names a module evaluated from a string or a registered source file.
pub(crate) fn has_inline_source(state: &LoopState, key: &str) -> bool {
    state.inline_sources.borrow().contains_key(key) || registered_source(&state.shared, key).is_some()
}

/// A registered source file for an extensionless or directory specifier, probed like Node.
fn probe_source_file(shared: &Shared, base: &str) -> Option<String> {
    const SUFFIXES: [&str; 8] = [".js", ".mjs", ".cjs", ".ts", ".json", "/index.js", "/index.mjs", "/index.ts"];
    let files = shared.source_files.read().unwrap_or_else(|poison| poison.into_inner());
    if files.is_empty() {
        return None;
    }
    let base = base.trim_end_matches(['/', '\\']);
    SUFFIXES
        .iter()
        .map(|suffix| normalize(Path::new(&format!("{base}{suffix}"))).display().to_string())
        .find(|key| files.contains_key(key))
}

/// The source of a file registered with `register_source_files`.
pub(crate) fn registered_source(shared: &Shared, key: &str) -> Option<std::sync::Arc<str>> {
    shared.source_files.read().unwrap_or_else(|poison| poison.into_inner()).get(key).cloned()
}

fn inline_source(state: &LoopState, key: &str) -> Option<String> {
    if let Some(source) = state.inline_sources.borrow().get(key) {
        return Some(source.clone());
    }
    let files = state.shared.source_files.read().unwrap_or_else(|poison| poison.into_inner());
    files.get(key).map(|source| source.to_string())
}

/// Make `name` (relative names resolve against the cwd) load from `source`; returns its key.
pub(crate) fn register_inline(name: &str, source: String) -> String {
    let state = state();
    let path = Path::new(name);
    let key = if path.is_absolute() { normalize(path) } else { normalize(&state.cwd.borrow().join(path)) };
    let key = key.display().to_string();
    state.inline_sources.borrow_mut().insert(key.clone(), source);
    key
}

struct PiResolver;

impl Resolver for PiResolver {
    fn resolve<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        if state().module_hooks.get() {
            let internal = state().internal(ctx)?;
            let hook: Function = internal.get("hookResolve")?;
            return hook.call((name, base));
        }
        resolve_key(name, Some(base)).map_err(|error| throw(ctx, &error))
    }
}

struct PiLoader;

impl Loader for PiLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        load(ctx, name).map_err(|error| throw(ctx, &error))
    }
}

fn load<'js>(ctx: &Ctx<'js>, key: &str) -> Result<Module<'js, Declared>, JsError> {
    let state = state();
    if state.module_hooks.get()
        && let Some(module) = hooked_load(ctx, key)?
    {
        return Ok(module);
    }
    if let Some(source) = inline_source(&state, key) {
        return load_source(ctx, key, Path::new(key), &source);
    }
    if key.starts_with("node:") || key == "pi:host" {
        let name = key.strip_prefix("node:").unwrap_or(key);
        return facade(ctx, key, "builtinFacade", (name.to_string(),));
    }
    let module = state.shared.virtual_modules.read().unwrap_or_else(|poison| poison.into_inner()).get(key).cloned();
    if let Some(module) = module {
        return load_virtual(ctx, key, module);
    }
    let path = PathBuf::from(key);
    if path.extension().is_some_and(|ext| ext == "json") {
        let text = std::fs::read_to_string(&path).map_err(|error| JsError::from_io(&error, "open", Some(key)))?;
        let value = ctx.json_parse(text).map_err(|error| caught(ctx, error))?;
        return facade(ctx, key, "valueFacade", (key.to_string(), value));
    }
    let source = std::fs::read_to_string(&path).map_err(|error| JsError::from_io(&error, "open", Some(key)))?;
    load_source(ctx, key, &path, &source)
}

/// Run `module.register` load hooks; `None` when no hook supplied the module.
fn hooked_load<'js>(ctx: &Ctx<'js>, key: &str) -> Result<Option<Module<'js, Declared>>, JsError> {
    let internal = state().internal(ctx).map_err(|error| caught(ctx, error))?;
    let hook: Function = internal.get("hookLoad").map_err(|error| caught(ctx, error))?;
    let result: Option<rquickjs::Object> = hook.call((key,)).map_err(|error| caught(ctx, error))?;
    let Some(result) = result else { return Ok(None) };
    let format: String = result.get("format").map_err(|error| caught(ctx, error))?;
    let source: Option<String> = result.get("source").map_err(|error| caught(ctx, error))?;
    let module = match (format.as_str(), source) {
        ("builtin", _) => facade(ctx, key, "builtinFacade", (key.strip_prefix("node:").unwrap_or(key).to_string(),))?,
        ("commonjs", source) => facade(ctx, key, "cjsFacade", (key.to_string(), source))?,
        ("json", Some(source)) => {
            let value = ctx.json_parse(source).map_err(|error| caught(ctx, error))?;
            facade(ctx, key, "valueFacade", (key.to_string(), value))?
        }
        (_, source) => load_source(ctx, key, Path::new(key), &source.unwrap_or_default())?,
    };
    Ok(Some(module))
}

fn load_source<'js>(ctx: &Ctx<'js>, key: &str, path: &Path, source: &str) -> Result<Module<'js, Declared>, JsError> {
    let transpiled = state().shared.transpiler.get_or_transpile(path, source)?;
    match transpiled.format {
        ModuleFormat::Esm => {
            let prefix = esm_prefix(key, &transpiled.free_globals);
            let map = transpiled.map.clone().map(Rc::new);
            register_source_map(key, map, prefix.chars().count() as u32);
            Module::declare(ctx.clone(), key, format!("{prefix}{}", transpiled.code))
                .map_err(|error| caught(ctx, error))
        }
        ModuleFormat::CommonJs => facade(ctx, key, "cjsFacade", (key.to_string(),)),
    }
}

fn load_virtual<'js>(ctx: &Ctx<'js>, key: &str, module: VirtualModule) -> Result<Module<'js, Declared>, JsError> {
    match module {
        VirtualModule::Source(source) => {
            register_source_map(key, None, 0);
            Module::declare(ctx.clone(), key, source).map_err(|error| caught(ctx, error))
        }
        VirtualModule::Json(_) | VirtualModule::Native(_) => facade(ctx, key, "virtualFacade", (key.to_string(),)),
    }
}

/// Declare a module whose source the bootstrap's `method` generates.
fn facade<'js, A>(ctx: &Ctx<'js>, key: &str, method: &str, args: A) -> Result<Module<'js, Declared>, JsError>
where
    A: rquickjs::function::IntoArgs<'js>,
{
    let internal = state().internal(ctx).map_err(|error| caught(ctx, error))?;
    let generate: Function = internal.get(method).map_err(|error| caught(ctx, error))?;
    let source: String = generate.call(args).map_err(|error| caught(ctx, error))?;
    register_source_map(key, None, 0);
    Module::declare(ctx.clone(), key, source).map_err(|error| caught(ctx, error))
}

fn js_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

/// One line put before an ES module: `import.meta` fields and any CommonJS globals it reads.
fn esm_prefix(key: &str, globals: &FreeGlobals) -> String {
    let path = Path::new(key);
    let file = js_string(key);
    let dir = js_string(&path.parent().map(|dir| dir.display().to_string()).unwrap_or_default());
    let url = url::Url::from_file_path(path).map(|url| url.to_string()).unwrap_or_else(|_| key.to_string());
    let mut prefix = format!(
        "import.meta.url = {}; import.meta.filename = {file}; import.meta.dirname = {dir}; \
         import.meta.resolve = (s) => globalThis.__pi_internal.resolveUrl(s, {file}); ",
        js_string(&url)
    );
    if globals.require {
        prefix.push_str(&format!("const require = globalThis.__pi_internal.createRequire({file}); "));
    }
    if globals.filename {
        prefix.push_str(&format!("const __filename = {file}; "));
    }
    if globals.dirname {
        prefix.push_str(&format!("const __dirname = {dir}; "));
    }
    prefix
}

/// Import a module synchronously (for `require` of an ES module), running jobs until it settles.
pub(crate) fn import_sync<'js>(ctx: &Ctx<'js>, key: &str) -> rquickjs::Result<Value<'js>> {
    let promise = Module::import(ctx, key)?;
    match promise.finish::<Value>() {
        Err(rquickjs::Error::WouldBlock) => Err(throw(
            ctx,
            &JsError::new(format!("require() cannot load {key}: it uses top-level await; use import() instead"))
                .with_code("ERR_REQUIRE_ASYNC_MODULE"),
        )),
        other => other,
    }
}
