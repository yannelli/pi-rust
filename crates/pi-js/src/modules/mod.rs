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
use crate::runtime::state::{LoopState, state};
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
    let from_path = from.map(Path::new).filter(|path| path.is_absolute());
    let base_dir = state.cwd.borrow().clone();
    if has_inline_source(&state, specifier) {
        return Ok(("file", specifier.to_string()));
    }
    if specifier.starts_with('.') {
        let dir = from_path.and_then(Path::parent).unwrap_or(&base_dir);
        let candidate = normalize(&dir.join(specifier)).display().to_string();
        if has_inline_source(&state, &candidate) {
            return Ok(("file", candidate));
        }
    }
    let virtual_modules = state.shared.virtual_modules.read().unwrap_or_else(|poison| poison.into_inner());
    let lookup = |name: &str| virtual_modules.canonical_name(name);
    let resolved = state.shared.resolver.resolve(specifier, from_path, &base_dir, kind, &lookup)?;
    Ok(match resolved {
        Resolved::Builtin(name) => ("builtin", name),
        Resolved::Virtual(name) => ("virtual", name),
        Resolved::File(path) => ("file", path.display().to_string()),
    })
}

/// Whether `key` names a module evaluated from a string or a registered source file.
pub(crate) fn has_inline_source(state: &LoopState, key: &str) -> bool {
    state.inline_sources.borrow().contains_key(key)
        || state.shared.source_files.read().unwrap_or_else(|poison| poison.into_inner()).contains_key(key)
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
