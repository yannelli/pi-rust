//! Facet bundles evaluated in the embedded QuickJS runtime (`pi-js`), in place of Node's `vm`.

mod bridge;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use pi_js::{HostFunction, JsError, JsHandle, JsRuntime, JsRuntimeOptions, VirtualModule};
use serde_json::Value;

use self::bridge::{Bridge, chord_error};
use super::loader::is_builtin;
use super::{ExportedFacet, FacetModule, FacetModuleEvaluator, ModuleExports};
use crate::context::{BACKGROUND_CONTEXT, Context};
use crate::error::{ChordError, ChordResult};
use crate::facets::{Facet, FacetEnvironment};
use crate::json::Json;

const CHORD_MODULE: &str = "@earendil-works/chord";
const CONTEXT_MODULE: &str = "@earendil-works/chord/context";

/// A JavaScript object kept alive for Rust, such as a local service a JavaScript facet provides.
pub struct JsObject {
    bridge: Arc<Bridge>,
    id: u64,
}

impl JsObject {
    pub(crate) fn new(bridge: Arc<Bridge>, id: u64) -> Self {
        Self { bridge, id }
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// Call a method; the JavaScript side receives `args` followed by a context.
    pub async fn call(&self, method: &str, args: Vec<Json>) -> ChordResult<Option<Json>> {
        self.call_with_context(method, args, &BACKGROUND_CONTEXT).await
    }

    /// Call a method; cancelling `context` aborts the context the method receives.
    pub async fn call_with_context(
        &self,
        method: &str,
        args: Vec<Json>,
        context: &Context,
    ) -> ChordResult<Option<Json>> {
        self.bridge.invoke(self.id, method, args, context).await
    }
}

impl Drop for JsObject {
    fn drop(&mut self) {
        self.bridge.post("release", vec![Value::from(self.id).into()]);
    }
}

impl std::fmt::Debug for JsObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsObject").field("id", &self.id).finish()
    }
}

struct JsFacet {
    id: String,
    object: u64,
    bridge: Arc<Bridge>,
}

impl Facet for JsFacet {
    fn id(&self) -> &str {
        &self.id
    }

    fn setup(&self, env: &FacetEnvironment) -> ChordResult<()> {
        let env_id = self.bridge.register_env(env.clone());
        let weak = Arc::downgrade(&self.bridge);
        env.own(move || {
            if let Some(bridge) = weak.upgrade() {
                bridge.forget_env(env_id);
            }
            Ok(())
        })?;
        let args = vec![Value::from(self.object).into(), Value::from(env_id).into()];
        self.bridge.api()?.blocking_call_method("setup", args).map_err(chord_error)?;
        Ok(())
    }
}

impl Drop for JsFacet {
    fn drop(&mut self) {
        self.bridge.post("release", vec![Value::from(self.object).into()]);
    }
}

/// Evaluates bundled CommonJS facets in QuickJS. `@earendil-works/chord` and its `context`
/// subpath resolve to embedded modules that forward to the Rust facet host.
pub struct QuickJsFacetEvaluator {
    bridge: Arc<Bridge>,
    generation: AtomicU64,
}

impl QuickJsFacetEvaluator {
    /// Start a dedicated runtime rooted at `cwd`, using the current Tokio runtime for host work.
    pub async fn new(cwd: impl Into<PathBuf>) -> ChordResult<Arc<Self>> {
        let mut options = JsRuntimeOptions::new(cwd);
        options.name = "pi-chord".into();
        options.tokio = tokio::runtime::Handle::try_current().ok();
        let runtime = JsRuntime::new(options).map_err(chord_error)?;
        Self::install(runtime.handle(), Some(runtime)).await
    }

    /// Use an existing runtime. It registers the chord modules and host functions there, so a
    /// runtime serves one evaluator.
    pub async fn with_handle(handle: JsHandle) -> ChordResult<Arc<Self>> {
        Self::install(handle, None).await
    }

    pub fn handle(&self) -> JsHandle {
        self.bridge.handle.clone()
    }

    async fn install(handle: JsHandle, runtime: Option<JsRuntime>) -> ChordResult<Arc<Self>> {
        let bridge = Bridge::new(handle.clone(), runtime);
        handle.register_virtual_module(CONTEXT_MODULE, VirtualModule::Source(include_str!("context.js").into()));
        handle.register_virtual_module(CHORD_MODULE, VirtualModule::Source(include_str!("chord.js").into()));
        let weak = Arc::downgrade(&bridge);
        handle.register_host_function(
            "chord",
            HostFunction::sync(move |args| {
                let bridge = weak.upgrade().ok_or_else(|| JsError::new("The chord facet host is closed"))?;
                bridge.dispatch(args)
            }),
        );
        let weak = Arc::downgrade(&bridge);
        handle.register_host_function(
            "chord.async",
            HostFunction::asynchronous(move |args| {
                let bridge = weak.upgrade();
                async move {
                    let bridge = bridge.ok_or_else(|| JsError::new("The chord facet host is closed"))?;
                    bridge.dispatch_async(args).await
                }
            }),
        );
        let namespace = handle.import(CHORD_MODULE).await.map_err(chord_error)?;
        let api = namespace.get_ref("__chordRuntime").await.map_err(chord_error)?;
        let _ = bridge.api.set(api);
        Ok(Arc::new(Self { bridge, generation: AtomicU64::new(1) }))
    }
}

fn decode_percent(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes.get(index + 1..index + 3).and_then(|pair| std::str::from_utf8(pair).ok());
        match (bytes[index], hex.and_then(|pair| u8::from_str_radix(pair, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                index += 3;
            }
            (byte, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn file_url_path(url: &str) -> String {
    let path = url.trim_start_matches("file://");
    let path = decode_percent(path.strip_prefix("localhost").unwrap_or(path));
    let bytes = path.as_bytes();
    let drive = cfg!(windows) && bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':';
    if drive { path[1..].replace('/', "\\") } else { path }
}

/// A resolver result as a `require` specifier: a path, a package, or a built-in module.
fn require_specifier(target: &str) -> ChordResult<String> {
    if Path::new(target).is_absolute() || is_builtin(target) {
        return Ok(target.to_string());
    }
    let scheme = target.split_once(':').map(|(scheme, _)| scheme).filter(|scheme| {
        let mut chars = scheme.chars();
        chars.next().is_some_and(|first| first.is_ascii_alphabetic())
            && chars.all(|char| char.is_ascii_alphanumeric() || "+-.".contains(char))
    });
    match scheme.map(str::to_ascii_lowercase).as_deref() {
        None => Ok(target.to_string()),
        Some("file") => Ok(file_url_path(target)),
        Some("node") => Ok(target.to_string()),
        Some(_) => Err(ChordError::error(format!(
            "Facet bundle external target must be a file, package, or built-in module: {target}"
        ))),
    }
}

fn js_text(value: &str) -> String {
    Value::from(value).to_string()
}

fn wrapper(module: &FacetModule) -> ChordResult<String> {
    let mut targets = Vec::new();
    for specifier in &module.external_imports {
        let resolved = module.resolve_external.as_ref().and_then(|resolve| resolve(specifier));
        let target = match resolved {
            Some(target) => require_specifier(&target)?,
            None => specifier.clone(),
        };
        targets.push(Value::Array(vec![specifier.as_str().into(), target.into()]));
    }
    let path = module.module_path.display().to_string();
    let directory = module.module_path.parent().map(|parent| parent.display().to_string()).unwrap_or_default();
    Ok(format!(
        "import {{ createRequire as __createRequire }} from \"node:module\";\n\
         import {{ __chordRuntime }} from \"{CHORD_MODULE}\";\n\
         const __targets = new Map({targets});\n\
         const __base = __createRequire({path});\n\
         const __targetFor = (specifier) => {{\n\
         \x20 const target = __targets.get(specifier);\n\
         \x20 if (target === undefined) throw new Error(`Facet bundle required undeclared external import: ${{specifier}}`);\n\
         \x20 return target;\n\
         }};\n\
         const __require = Object.assign((specifier) => __base(__targetFor(specifier)), {{\n\
         \x20 resolve: (specifier) => __base.resolve(__targetFor(specifier)),\n\
         }});\n\
         const __module = {{ exports: Object.create(null) }};\n\
         function __facetModule(exports, require, module, __filename, __dirname) {{\n{source}\n}}\n\
         Reflect.apply(__facetModule, __module.exports, [__module.exports, __require, __module, {path}, {directory}]);\n\
         export default __chordRuntime.describeModule(__module.exports);\n",
        targets = Value::Array(targets),
        path = js_text(&path),
        directory = js_text(&directory),
        source = module.source,
    ))
}

fn exported(bridge: &Arc<Bridge>, candidate: &Value) -> ExportedFacet {
    let id = candidate.get("id").and_then(Value::as_str).map(str::to_string);
    match (candidate.get("invalid").and_then(Value::as_str), id) {
        (Some("setup"), Some(id)) => ExportedFacet::MissingSetup(id),
        (None, Some(id)) => {
            let object = candidate.get("object").and_then(Value::as_u64).unwrap_or_default();
            ExportedFacet::Facet(Arc::new(JsFacet { id, object, bridge: bridge.clone() }))
        }
        _ => ExportedFacet::InvalidId,
    }
}

#[async_trait]
impl FacetModuleEvaluator for QuickJsFacetEvaluator {
    async fn evaluate(&self, module: FacetModule) -> ChordResult<ModuleExports> {
        let source = wrapper(&module)?;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst);
        let name = format!("{}.chord-{generation}.mjs", module.module_path.display());
        let namespace = self.bridge.handle.eval_module(name, source).await.map_err(chord_error)?;
        let described = namespace.get("default").await.map_err(chord_error)?.into_json();
        if described.get("module").and_then(Value::as_bool) != Some(true) {
            return Ok(ModuleExports::NotAModule);
        }
        let candidates = described.get("facets").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(ModuleExports::Facets(candidates.iter().map(|candidate| exported(&self.bridge, candidate)).collect()))
    }
}
