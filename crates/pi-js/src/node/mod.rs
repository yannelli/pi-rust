//! Node and web compatibility: native functions plus the JavaScript built-in modules.

mod builtins;
mod child;
mod context;
mod core;
mod crypto;
mod dgram;
mod dns;
pub(crate) mod encoding;
pub(crate) mod fetch;
pub(crate) mod fs;
mod fs_ops;
mod fs_output;
mod handles;
mod http;
mod http2;
mod http2_stream;
mod net;
mod net_connect;
mod net_server;
mod os;
mod pipe;
mod ports;
mod sqlite;
mod sqlite_fn;
pub(crate) mod sys;
mod tls;
pub(crate) mod vm;
mod wasm;
mod wasm_objects;
mod watch;
mod worker;
mod x509;
mod zlib;

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
    native.set("child", child::natives(ctx)?)?;
    native.set("pipe", pipe::natives(ctx)?)?;
    native.set("watch", watch::natives(ctx)?)?;
    native.set("fetch", fetch::natives(ctx)?)?;
    native.set("zlib", zlib::natives(ctx)?)?;
    native.set("net", net::natives(ctx)?)?;
    native.set("dns", dns::natives(ctx)?)?;
    native.set("dgram", dgram::natives(ctx)?)?;
    native.set("tls", tls::natives(ctx)?)?;
    native.set("http", http::natives(ctx)?)?;
    native.set("http2", http2::natives(ctx)?)?;
    native.set("vm", vm::natives(ctx)?)?;
    native.set("worker", ports::natives(ctx)?)?;
    native.set("sqlite", sqlite::natives(ctx)?)?;
    native.set("wasm", wasm::natives(ctx)?)?;
    native.set("asyncContext", context::natives(ctx)?)?;
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

/// Built-in modules that load, without the `node:` prefix.
pub(crate) fn supported_modules() -> impl Iterator<Item = &'static str> {
    builtins::supported()
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
