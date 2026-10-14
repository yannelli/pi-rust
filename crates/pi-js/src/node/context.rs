//! Natives for async context (`node:async_hooks` stores) and `node:v8` heap statistics.

use rquickjs::{Ctx, Object, Persistent, Value, qjs};

use super::set_fn;
use crate::runtime::state::state;

/// QuickJS's memory accounting for the runtime that owns `ctx`.
#[allow(unsafe_code)]
fn memory_usage(ctx: &Ctx<'_>) -> qjs::JSMemoryUsage {
    // SAFETY: `ctx` is a live context; QuickJS fills the whole struct.
    unsafe {
        let runtime = qjs::JS_GetRuntime(ctx.as_raw().as_ptr());
        let mut usage = std::mem::MaybeUninit::<qjs::JSMemoryUsage>::zeroed();
        qjs::JS_ComputeMemoryUsage(runtime, usage.as_mut_ptr());
        usage.assume_init()
    }
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "get", |ctx: Ctx<'js>| -> rquickjs::Result<Value<'js>> {
        match state().async_frame.borrow().clone() {
            Some(frame) => frame.restore(&ctx),
            None => Ok(Value::new_undefined(ctx)),
        }
    })?;
    set_fn(&object, "set", |ctx: Ctx<'js>, frame: Value<'js>| {
        let frame = (!frame.is_undefined()).then(|| Persistent::save(&ctx, frame));
        *state().async_frame.borrow_mut() = frame;
    })?;
    set_fn(&object, "memory", |ctx: Ctx<'js>| {
        let usage = memory_usage(&ctx);
        let fields = [
            ("mallocSize", usage.malloc_size),
            ("mallocLimit", usage.malloc_limit),
            ("memoryUsed", usage.memory_used_size),
            ("mallocCount", usage.malloc_count),
            ("atomSize", usage.atom_size),
            ("strSize", usage.str_size),
            ("objSize", usage.obj_size),
            ("propSize", usage.prop_size),
            ("shapeSize", usage.shape_size),
            ("funcSize", usage.js_func_size),
            ("funcCodeSize", usage.js_func_code_size),
            ("funcCount", usage.js_func_count),
            ("cFuncCount", usage.c_func_count),
            ("binarySize", usage.binary_object_size),
        ];
        let object = Object::new(ctx.clone())?;
        for (name, value) in fields {
            object.set(name, value as f64)?;
        }
        Ok::<_, rquickjs::Error>(object)
    })?;
    Ok(object)
}
