//! Natives for `node:vm`: extra QuickJS contexts in the same runtime (so values pass between
//! them), and evaluation under a deadline that the runtime's interrupt handler enforces.
#![allow(unsafe_code)]

use std::ffi::CString;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rquickjs::context::EvalOptions;
use rquickjs::{Ctx, Object, Value, qjs};

use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::throw;
use crate::runtime::state::state;

/// A context made with `JS_NewContext`; freed with the other resources before the runtime.
struct VmContext(NonNull<qjs::JSContext>);

impl Drop for VmContext {
    fn drop(&mut self) {
        // SAFETY: this holds one reference to a live context of the runtime being torn down.
        unsafe { qjs::JS_FreeContext(self.0.as_ptr()) }
    }
}

fn now_nanos() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    u64::try_from(EPOCH.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(u64::MAX).max(1)
}

/// Whether a `vm` timeout has passed; 0 means no timeout is set.
pub(crate) fn deadline_passed(deadline: &AtomicU64) -> bool {
    let value = deadline.load(Ordering::Relaxed);
    value != 0 && now_nanos() >= value
}

fn create<'js>(ctx: Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    // SAFETY: the runtime lock is held while a native function runs; the new context gets the
    // caller's lifetime because both belong to the same runtime.
    let (other, id) = unsafe {
        let raw = qjs::JS_NewContext(qjs::JS_GetRuntime(ctx.as_raw().as_ptr()));
        let raw = NonNull::new(raw).ok_or_else(|| throw(&ctx, &JsError::new("Failed to create a vm context")))?;
        let other = Ctx::<'js>::from_raw(raw);
        (other, state().insert_resource(Box::new(VmContext(raw))))
    };
    let result = Object::new(ctx.clone())?;
    result.set("id", id)?;
    result.set("global", other.globals())?;
    Ok(result)
}

fn context_for<'js>(ctx: &Ctx<'js>, id: u64) -> rquickjs::Result<Ctx<'js>> {
    if id == 0 {
        return Ok(ctx.clone());
    }
    let raw = state().with_resource(id, |context: &mut VmContext| context.0).map_err(|error| throw(ctx, &error))?;
    // SAFETY: as in `create`; the resource keeps the context alive.
    Ok(unsafe { Ctx::from_raw(raw) })
}

/// Evaluate a script in context `id` (0 is the main context), with an optional timeout in ms.
fn run<'js>(
    ctx: Ctx<'js>,
    id: u64,
    code: String,
    filename: String,
    timeout: Option<f64>,
    drain: Option<bool>,
) -> rquickjs::Result<Value<'js>> {
    let target = context_for(&ctx, id)?;
    let shared = state().shared.clone();
    let previous = shared.vm_deadline.load(Ordering::Relaxed);
    let own = timeout.map(|ms| now_nanos().saturating_add((ms.max(0.0) * 1e6) as u64));
    if let Some(own) = own {
        shared.vm_deadline.store(if previous == 0 { own } else { previous.min(own) }, Ordering::Relaxed);
    }
    let mut options = EvalOptions::default();
    options.global = true;
    options.strict = false;
    options.filename = Some(filename);
    let mut result = target.eval_with_options::<Value, _>(code, options);
    if result.is_ok() && drain.unwrap_or(false) {
        while target.execute_pending_job() {}
        if deadline_passed(&shared.vm_deadline) {
            result = Err(rquickjs::Error::Exception);
        }
    }
    let timed_out = own.is_some_and(|own| now_nanos() >= own);
    shared.vm_deadline.store(previous, Ordering::Relaxed);
    match result {
        Err(rquickjs::Error::Exception) if timed_out => {
            let _ = ctx.catch();
            let ms = timeout.unwrap_or_default();
            let message = format!("Script execution timed out after {ms}ms");
            Err(throw(&ctx, &JsError::new(message).with_code("ERR_SCRIPT_EXECUTION_TIMEOUT")))
        }
        other => other,
    }
}

/// Parse `code` as a script without running it, so `new vm.Script` throws syntax errors.
fn check(ctx: Ctx<'_>, code: String, filename: String) -> rquickjs::Result<()> {
    let length = code.len();
    let code = CString::new(code).map_err(|_| throw(&ctx, &JsError::new("Invalid script: NUL character")))?;
    let filename = CString::new(filename).unwrap_or_default();
    let flags = (qjs::JS_EVAL_TYPE_GLOBAL | qjs::JS_EVAL_FLAG_COMPILE_ONLY) as i32;
    // SAFETY: `code` is NUL-terminated at `length`, as JS_Eval requires; the result is freed.
    unsafe {
        let raw = ctx.as_raw().as_ptr();
        let value = qjs::JS_Eval(raw, code.as_ptr(), length as _, filename.as_ptr(), flags);
        if qjs::JS_IsException(value) {
            return Err(rquickjs::Error::Exception);
        }
        qjs::JS_FreeValue(raw, value);
    }
    Ok(())
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "createContext", create)?;
    set_fn(&object, "run", run)?;
    set_fn(&object, "check", check)?;
    set_fn(&object, "dispose", |id: u64| drop(state().resources.borrow_mut().remove(&id)))?;
    Ok(object)
}
