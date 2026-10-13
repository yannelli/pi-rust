//! JavaScript sources of the built-in modules, compiled on first `require`.

use rquickjs::context::EvalOptions;
use rquickjs::{Ctx, Function, Object};

use super::set_fn;
use crate::error::JsError;
use crate::resolve::NODE_BUILTINS;
use crate::runtime::convert::throw;

/// `(name, source)`. Names are Node's without `node:`; `internal/` names are private helpers.
const SOURCES: &[(&str, &str)] = &[
    ("pi:host", include_str!("../js/host.js")),
    ("path", include_str!("../js/path.js")),
    ("internal/path_win32", include_str!("../js/path_win32.js")),
    ("path/posix", "module.exports = require('path').posix;"),
    ("path/win32", "module.exports = require('path').win32;"),
    ("events", include_str!("../js/events.js")),
    ("internal/inspect", include_str!("../js/inspect.js")),
    ("internal/deep_equal", include_str!("../js/deep_equal.js")),
    ("util", include_str!("../js/util.js")),
    ("util/types", "module.exports = require('util').types;"),
    ("sys", "module.exports = require('util');"),
    ("timers", include_str!("../js/timers.js")),
    ("timers/promises", "module.exports = require('timers').promises;"),
    ("process", include_str!("../js/process.js")),
    ("console", include_str!("../js/console.js")),
    ("module", include_str!("../js/module.js")),
    ("perf_hooks", include_str!("../js/perf_hooks.js")),
];

/// Every module name `require` accepts, for `module.builtinModules`.
pub(crate) fn supported() -> impl Iterator<Item = &'static str> {
    SOURCES.iter().map(|(name, _)| *name).filter(|name| !name.starts_with("internal/") && *name != "pi:host")
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "compile", compile)?;
    set_fn(&object, "list", || supported().map(str::to_string).collect::<Vec<_>>())?;
    Ok(object)
}

fn compile<'js>(ctx: Ctx<'js>, name: String) -> rquickjs::Result<Function<'js>> {
    let Some((_, source)) = SOURCES.iter().find(|(known, _)| *known == name) else {
        let error = if NODE_BUILTINS.contains(&name.as_str()) {
            JsError::unsupported(&format!("Node module 'node:{name}'"))
        } else {
            JsError::new(format!("No such built-in module: {name}")).with_code("ERR_UNKNOWN_BUILTIN_MODULE")
        };
        return Err(throw(&ctx, &error));
    };
    let mut options = EvalOptions::default();
    options.strict = false;
    options.filename = Some(if name.contains(':') { name.clone() } else { format!("node:{name}") });
    ctx.eval_with_options(
        format!("(function (module, exports, require, native, internal) {{ \"use strict\"; {source}\n}})"),
        options,
    )
}
