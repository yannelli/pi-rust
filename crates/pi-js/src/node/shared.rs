//! SharedArrayBuffer memory that every runtime in the process can use. Each runtime allocates
//! SharedArrayBuffer memory through these QuickJS hooks, so a buffer posted to a worker is rebuilt
//! there over the same bytes. Atomics.wait and notify use one waiter list for the whole process.
#![allow(unsafe_code)]

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use rquickjs::{Ctx, Value, qjs};

use crate::error::JsError;
use crate::runtime::convert::throw;

/// Bytes before the data: the reference count, then the size of the whole allocation.
const HEADER: usize = 16;

/// QuickJS's class id for SharedArrayBuffer, read once from a real one.
static CLASS: AtomicU32 = AtomicU32::new(0);

/// SAFETY (for the helpers below): `data` must come from `sab_alloc` and still be referenced.
unsafe fn base(data: *mut c_void) -> *mut u8 {
    unsafe { data.cast::<u8>().sub(HEADER) }
}

unsafe fn count<'a>(data: *mut c_void) -> &'a AtomicUsize {
    unsafe { &*base(data).cast::<AtomicUsize>() }
}

unsafe fn allocated(data: *mut c_void) -> usize {
    unsafe { base(data).add(std::mem::size_of::<usize>()).cast::<usize>().read() - HEADER }
}

unsafe extern "C" fn sab_alloc(_: *mut c_void, size: qjs::size_t) -> *mut c_void {
    let size = size as usize;
    let Ok(layout) = Layout::from_size_align(size.saturating_add(HEADER), HEADER) else { return null_mut() };
    // SAFETY: the layout has a non-zero size; the header fits in the first HEADER bytes.
    unsafe {
        let start = alloc_zeroed(layout);
        if start.is_null() {
            return null_mut();
        }
        start.cast::<AtomicUsize>().write(AtomicUsize::new(1));
        start.add(std::mem::size_of::<usize>()).cast::<usize>().write(layout.size());
        start.add(HEADER).cast()
    }
}

unsafe extern "C" fn sab_dup(_: *mut c_void, data: *mut c_void) {
    unsafe { count(data) }.fetch_add(1, Ordering::Relaxed);
}

unsafe extern "C" fn sab_free(_: *mut c_void, data: *mut c_void) {
    // SAFETY: the last reference frees the block with the layout `sab_alloc` used.
    unsafe {
        if count(data).fetch_sub(1, Ordering::AcqRel) == 1 {
            let size = allocated(data) + HEADER;
            dealloc(base(data), Layout::from_size_align_unchecked(size, HEADER));
        }
    }
}

/// Route this runtime's SharedArrayBuffer memory through the shared allocator, and let
/// Atomics.wait block, as Node allows on every thread. Runs before any script.
pub(crate) fn install(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    let functions = qjs::JSSharedArrayBufferFunctions {
        sab_alloc: Some(sab_alloc),
        sab_free: Some(sab_free),
        sab_dup: Some(sab_dup),
        sab_opaque: null_mut(),
    };
    // SAFETY: `ctx` is live; QuickJS copies the struct.
    unsafe {
        let runtime = qjs::JS_GetRuntime(ctx.as_raw().as_ptr());
        qjs::JS_SetSharedArrayBufferFunctions(runtime, &functions);
        qjs::JS_SetCanBlock(runtime, true);
    }
    let probe: Value = ctx.eval("new SharedArrayBuffer(1)")?;
    // SAFETY: reading the class id of a live value.
    CLASS.store(unsafe { qjs::JS_GetClassID(probe.as_raw()) }, Ordering::Relaxed);
    Ok(())
}

/// One reference to shared memory, held while a message carries it between threads.
pub(crate) struct SharedBlock {
    data: *mut c_void,
    length: usize,
    max: usize,
}

// SAFETY: the memory is reference counted with atomics and never moves.
unsafe impl Send for SharedBlock {}

impl Clone for SharedBlock {
    fn clone(&self) -> Self {
        unsafe { sab_dup(null_mut(), self.data) };
        Self { ..*self }
    }
}

impl Drop for SharedBlock {
    fn drop(&mut self) {
        unsafe { sab_free(null_mut(), self.data) };
    }
}

impl SharedBlock {
    /// Take a reference to a SharedArrayBuffer's memory. `max` is its maxByteLength when it is
    /// growable; it never exceeds what was allocated.
    pub(crate) fn of(ctx: &Ctx<'_>, value: &Value<'_>, max: Option<usize>) -> rquickjs::Result<Self> {
        let not_shared = || throw(ctx, &JsError::type_error("Expected a SharedArrayBuffer"));
        // SAFETY: the class check makes sure the data came from `sab_alloc`.
        unsafe {
            if qjs::JS_GetClassID(value.as_raw()) != CLASS.load(Ordering::Relaxed) {
                return Err(not_shared());
            }
            let mut length = 0;
            let data = qjs::JS_GetArrayBuffer(ctx.as_raw().as_ptr(), &mut length, value.as_raw());
            if data.is_null() {
                return Err(not_shared());
            }
            let data = data.cast::<c_void>();
            let length = length as usize;
            let max = max.unwrap_or(0).min(allocated(data));
            sab_dup(null_mut(), data);
            Ok(Self { data, length, max: if max > length { max } else { 0 } })
        }
    }

    /// A SharedArrayBuffer in this runtime over the same memory.
    pub(crate) fn adopt<'js>(&self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        // SAFETY: QuickJS takes its own reference through `sab_dup`.
        unsafe {
            let raw = ctx.as_raw().as_ptr();
            let value =
                qjs::JS_NewArrayBuffer(raw, self.data.cast(), self.length as _, self.max as _, None, null_mut(), true);
            if qjs::JS_IsException(value) {
                return Err(rquickjs::Error::Exception);
            }
            Ok(Value::from_raw(ctx.clone(), value))
        }
    }
}
