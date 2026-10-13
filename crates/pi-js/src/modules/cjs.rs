//! Native functions behind the CommonJS loader in the bootstrap.

use std::path::Path;
use std::rc::Rc;

use rquickjs::context::EvalOptions;
use rquickjs::function::Opt;
use rquickjs::{Ctx, Function, Object, Value};

use super::{import_sync, resolve_kind};
use crate::error::JsError;
use crate::resolve::ResolveKind;
use crate::runtime::JsScope;
use crate::runtime::convert::{from_json, throw};
use crate::runtime::state::state;
use crate::transpile::{ModuleFormat, register_source_map};
use crate::virtual_modules::VirtualModule;

const WRAPPER: &str = "(function (exports, require, module, __filename, __dirname) { ";

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    object.set(
        "resolve",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, specifier: String, parent: Opt<Option<String>>, kind: Opt<String>| {
                let kind = if kind.0.as_deref() == Some("import") { ResolveKind::Import } else { ResolveKind::Require };
                let (kind, key) = resolve_kind(&specifier, parent.0.flatten().as_deref(), kind)
                    .map_err(|error| throw(&ctx, &error))?;
                let result = Object::new(ctx.clone())?;
                result.set("kind", kind)?;
                result.set("key", key)?;
                Ok::<_, rquickjs::Error>(result)
            },
        )?,
    )?;
    object.set("compile", Function::new(ctx.clone(), compile)?)?;
    object.set("importSync", Function::new(ctx.clone(), |ctx: Ctx<'js>, key: String| import_sync(&ctx, &key))?)?;
    object.set("virtualValue", Function::new(ctx.clone(), virtual_value)?)?;
    Ok(object)
}

/// `{ format: "cjs", fn }`, `{ format: "esm" }`, or `{ format: "json", text }` for a file.
fn compile<'js>(ctx: Ctx<'js>, filename: String) -> rquickjs::Result<Object<'js>> {
    let result = Object::new(ctx.clone())?;
    let state = state();
    if super::has_inline_source(&state, &filename) {
        result.set("format", "esm")?;
        return Ok(result);
    }
    let path = Path::new(&filename);
    let source = std::fs::read_to_string(path)
        .map_err(|error| throw(&ctx, &JsError::from_io(&error, "open", Some(&filename))))?;
    if path.extension().is_some_and(|ext| ext == "json") {
        result.set("format", "json")?;
        result.set("text", source)?;
        return Ok(result);
    }
    let transpiled = state.shared.transpiler.get_or_transpile(path, &source).map_err(|error| throw(&ctx, &error))?;
    if transpiled.format == ModuleFormat::Esm {
        result.set("format", "esm")?;
        return Ok(result);
    }
    register_source_map(&filename, transpiled.map.clone().map(Rc::new), WRAPPER.chars().count() as u32);
    let mut options = EvalOptions::default();
    options.strict = false;
    options.filename = Some(filename.clone());
    let wrapper: Function = ctx.eval_with_options(format!("{WRAPPER}{}\n}})", transpiled.code), options)?;
    result.set("format", "cjs")?;
    result.set("fn", wrapper)?;
    Ok(result)
}

/// `{ source: true }` for source-backed virtual modules, else `{ value }` built once by the caller.
fn virtual_value<'js>(ctx: Ctx<'js>, key: String) -> rquickjs::Result<Object<'js>> {
    let module = state().shared.virtual_modules.read().unwrap_or_else(|poison| poison.into_inner()).get(&key).cloned();
    let result = Object::new(ctx.clone())?;
    match module {
        None => return Err(throw(&ctx, &JsError::new(format!("Unknown virtual module {key}")))),
        Some(VirtualModule::Source(_)) => result.set("source", true)?,
        Some(VirtualModule::Json(json)) => result.set("value", from_json(&ctx, &json)?)?,
        Some(VirtualModule::Native(build)) => {
            let object = build(&JsScope::new(ctx.clone())).map_err(|error| throw(&ctx, &error))?;
            result.set::<_, Value>("value", object.into_value())?;
        }
    }
    Ok(result)
}
