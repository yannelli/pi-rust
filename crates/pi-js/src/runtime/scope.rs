//! `JsScope`: the context handed to Rust closures running on the JavaScript thread.

use std::future::Future;
use std::path::PathBuf;

use rquickjs::context::EvalOptions;
use rquickjs::function::Rest;
use rquickjs::{Ctx, Function, Module, Promise, Value};

use super::convert::{self, caught, error_value, from_host, throw, to_host};
use super::state::{PendingPromise, state};
use super::{JsHandle, JsRef};
use crate::error::JsError;
use crate::host::{HostFunction, HostValue};
use crate::modules;

/// Access to the JavaScript context from Rust code running on its thread.
pub struct JsScope<'js> {
    ctx: Ctx<'js>,
}

impl<'js> JsScope<'js> {
    pub(crate) fn new(ctx: Ctx<'js>) -> Self {
        Self { ctx }
    }

    pub fn ctx(&self) -> &Ctx<'js> {
        &self.ctx
    }

    pub fn handle(&self) -> JsHandle {
        JsHandle::from_state(&state())
    }

    /// The runtime's working directory (`process.cwd()`).
    pub fn cwd(&self) -> PathBuf {
        state().cwd.borrow().clone()
    }

    /// Keep a value alive for Rust until the returned reference is dropped.
    pub fn persist(&self, value: Value<'js>) -> JsRef {
        convert::persist(&self.ctx, value)
    }

    pub fn value(&self, reference: &JsRef) -> Result<Value<'js>, JsError> {
        convert::restore(&self.ctx, reference)
    }

    pub fn to_host(&self, value: Value<'js>) -> Result<HostValue, JsError> {
        to_host(&self.ctx, value)
    }

    pub fn from_host(&self, value: HostValue) -> Result<Value<'js>, JsError> {
        from_host(&self.ctx, value)
    }

    pub fn to_json(&self, value: Value<'js>) -> Result<serde_json::Value, JsError> {
        convert::to_json(&self.ctx, value)
    }

    pub fn from_json(&self, value: &serde_json::Value) -> Result<Value<'js>, JsError> {
        convert::from_json(&self.ctx, value).map_err(|error| caught(&self.ctx, error))
    }

    /// A JavaScript error object (with `code` and extra data) for a Rust error.
    pub fn error_value(&self, error: &JsError) -> Result<Value<'js>, JsError> {
        error_value(&self.ctx, error).map_err(|failure| caught(&self.ctx, failure))
    }

    /// Throw `error` from a native function: `return Err(scope.throw(&error))`.
    pub fn throw(&self, error: &JsError) -> rquickjs::Error {
        throw(&self.ctx, error)
    }

    /// The pending JavaScript exception behind an rquickjs error, with mapped stack.
    pub fn caught(&self, error: rquickjs::Error) -> JsError {
        caught(&self.ctx, error)
    }

    /// Run `future` on tokio and return a promise settled on this thread with `convert`.
    pub fn spawn<T, Fut, C>(&self, future: Fut, convert: C) -> Result<Promise<'js>, JsError>
    where
        T: Send + 'static,
        Fut: Future<Output = Result<T, JsError>> + Send + 'static,
        C: for<'a> FnOnce(&Ctx<'a>, T) -> Result<Value<'a>, JsError> + Send + 'static,
    {
        spawn_op(&self.ctx, future, convert).map_err(|error| caught(&self.ctx, error))
    }

    /// Wrap a host function as a JavaScript function.
    pub fn host_function(&self, function: HostFunction) -> Result<Function<'js>, JsError> {
        host_function(&self.ctx, function).map_err(|error| caught(&self.ctx, error))
    }

    pub(crate) fn eval_script(&self, source: &str, filename: &str) -> Result<Value<'js>, JsError> {
        let mut options = EvalOptions::default();
        options.strict = false;
        options.filename = Some(filename.into());
        self.ctx.eval_with_options(source, options).map_err(|error| caught(&self.ctx, error))
    }

    /// A promise for the namespace of the module `specifier` resolves to from the cwd.
    pub(crate) fn import_promise(&self, specifier: &str) -> Result<Value<'js>, JsError> {
        let key = modules::resolve_key(specifier, None)?;
        Module::import(&self.ctx, key).map(Promise::into_value).map_err(|error| caught(&self.ctx, error))
    }

    pub(crate) fn module_promise(&self, name: &str, source: String) -> Result<Value<'js>, JsError> {
        let key = modules::register_inline(name, source);
        Module::import(&self.ctx, key).map(Promise::into_value).map_err(|error| caught(&self.ctx, error))
    }
}

/// Create a promise whose resolvers wait in the loop state under the returned id.
pub(crate) fn pending_promise<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<(u64, Promise<'js>)> {
    let (promise, resolve, reject) = ctx.promise()?;
    let state = state();
    let id = state.next_id();
    let pending = PendingPromise {
        resolve: rquickjs::Persistent::save(ctx, resolve),
        reject: rquickjs::Persistent::save(ctx, reject),
    };
    state.promises.borrow_mut().insert(id, pending);
    Ok((id, promise))
}

/// Settle a promise made by [`pending_promise`].
pub(crate) fn settle_promise<'js>(ctx: &Ctx<'js>, id: u64, result: Result<Value<'js>, JsError>) {
    let Some(pending) = state().promises.borrow_mut().remove(&id) else { return };
    let outcome = match result {
        Ok(value) => pending.resolve.restore(ctx).and_then(|resolve| resolve.call::<_, ()>((value,))),
        Err(error) => error_value(ctx, &error)
            .and_then(|value| pending.reject.restore(ctx).and_then(|reject| reject.call::<_, ()>((value,)))),
    };
    if let Err(error) = outcome {
        let _ = caught(ctx, error);
    }
}

/// Run `future` on tokio and settle the returned promise on the JavaScript thread.
pub(crate) fn spawn_op<'js, T, Fut, C>(ctx: &Ctx<'js>, future: Fut, convert: C) -> rquickjs::Result<Promise<'js>>
where
    T: Send + 'static,
    Fut: Future<Output = Result<T, JsError>> + Send + 'static,
    C: for<'a> FnOnce(&Ctx<'a>, T) -> Result<Value<'a>, JsError> + Send + 'static,
{
    let (id, promise) = pending_promise(ctx)?;
    let loop_state = state();
    loop_state.add_busy();
    let sender = loop_state.sender.clone();
    loop_state.shared.tokio.spawn(async move {
        let result = future.await;
        let task: super::state::Task = Box::new(move |ctx| {
            state().remove_busy();
            settle_promise(ctx, id, result.and_then(|value| convert(ctx, value)));
        });
        let _ = sender.send(super::state::Message::Task(task));
    });
    Ok(promise)
}

pub(crate) fn host_function<'js>(ctx: &Ctx<'js>, function: HostFunction) -> rquickjs::Result<Function<'js>> {
    Function::new(ctx.clone(), move |ctx: Ctx<'js>, args: Rest<Value<'js>>| call_host(&ctx, &function, args.0))
}

pub(crate) fn call_host<'js>(
    ctx: &Ctx<'js>,
    function: &HostFunction,
    args: Vec<Value<'js>>,
) -> rquickjs::Result<Value<'js>> {
    let args = args.into_iter().map(|arg| to_host(ctx, arg)).collect::<Result<Vec<_>, _>>();
    let args = args.map_err(|error| throw(ctx, &error))?;
    match function {
        HostFunction::Sync(function) => {
            let result = function(args).map_err(|error| throw(ctx, &error))?;
            from_host(ctx, result).map_err(|error| throw(ctx, &error))
        }
        HostFunction::Async(function) => Ok(spawn_op(ctx, function(args), from_host)?.into_value()),
    }
}
