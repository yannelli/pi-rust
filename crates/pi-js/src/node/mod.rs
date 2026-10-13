//! Node and web compatibility: native functions plus the JavaScript built-in modules.

mod builtins;
mod core;
mod crypto;
pub(crate) mod encoding;
pub(crate) mod fs;
mod fs_ops;
mod fs_output;
mod os;

use rquickjs::context::EvalOptions;
use rquickjs::{Ctx, Function, Object, Persistent};

use crate::runtime::state::state;

const BOOTSTRAP: &str = include_str!("../js/bootstrap.js");

/// Install natives, run the bootstrap, and keep its internal API object.
pub(crate) fn install(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    let native = Object::new(ctx.clone())?;
    native.set("modules", crate::modules::cjs::natives(ctx)?)?;
    native.set("builtins", builtins::natives(ctx)?)?;
    native.set("encoding", encoding::natives(ctx)?)?;
    native.set("crypto", crypto::natives(ctx)?)?;
    native.set("fs", fs::natives(ctx)?)?;
    native.set("os", os::natives(ctx)?)?;
    core::install(ctx, &native)?;
    let config = core::config(ctx)?;
    let mut options = EvalOptions::default();
    options.strict = false;
    options.filename = Some("node:internal/bootstrap".into());
    let bootstrap: Function = ctx.eval_with_options(BOOTSTRAP, options)?;
    let internal: Object = bootstrap.call((native, config))?;
    *state().internal.borrow_mut() = Some(Persistent::save(ctx, internal));
    Ok(())
}

/// Define `name` on `object` as a native function.
pub(crate) fn set_fn<'js, P>(
    object: &Object<'js>,
    name: &str,
    function: impl rquickjs::function::IntoJsFunc<'js, P> + 'js,
) -> rquickjs::Result<()> {
    let function = Function::new(object.ctx().clone(), function)?.with_name(name)?;
    object.set(name, function)
}
