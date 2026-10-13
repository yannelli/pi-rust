//! C ABI for native hosts such as Swift (`include/pi_embedded.h`). Each handle owns a tokio
//! runtime and a [`NativeBridge`]; strings cross the boundary as NUL-terminated UTF-8 that is
//! only valid for the duration of the call.

// The workspace denies `unsafe_code`; this module alone allows it, because a C ABI must
// dereference host-supplied pointers. Every unsafe block states the caller contract it relies on.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Arc;

use crate::native::{NativeBridge, NativeHost};

/// `call(context, id, method, payload_json)`: a core request or a `command.result`.
pub type PiHostCallFn = extern "C" fn(*mut c_void, *const c_char, *const c_char, *const c_char);
/// `event(context, event_json)`: a session event or host error.
pub type PiHostEventFn = extern "C" fn(*mut c_void, *const c_char);

/// The host context pointer, passed back verbatim to the callbacks.
struct HostContext(*mut c_void);

// SAFETY: the header requires `context` and both callbacks to be usable from any thread.
unsafe impl Send for HostContext {}
// SAFETY: as above; the core never dereferences `context` itself.
unsafe impl Sync for HostContext {}

struct CallbackHost {
    call: PiHostCallFn,
    event: PiHostEventFn,
    context: HostContext,
}

/// Interior NUL bytes cannot cross a C string boundary; they are dropped.
fn c_string(value: &str) -> CString {
    CString::new(value.replace('\0', "")).unwrap_or_default()
}

impl NativeHost for CallbackHost {
    fn call(&self, id: &str, method: &str, payload: &str) {
        let (id, method, payload) = (c_string(id), c_string(method), c_string(payload));
        (self.call)(self.context.0, id.as_ptr(), method.as_ptr(), payload.as_ptr());
    }

    fn event(&self, event: &str) {
        let event = c_string(event);
        (self.event)(self.context.0, event.as_ptr());
    }
}

/// Opaque handle returned by [`pi_embedded_new`].
pub struct PiEmbedded {
    runtime: Option<tokio::runtime::Runtime>,
    bridge: NativeBridge,
}

/// # Safety
/// `value` must be null or a NUL-terminated string valid for this call. Invalid UTF-8 reads as `None`.
unsafe fn read(value: *const c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    // SAFETY: non-null and NUL-terminated per the function contract.
    unsafe { CStr::from_ptr(value) }.to_str().ok().map(String::from)
}

/// Create a bridge, or null when its runtime cannot start. Free it with [`pi_embedded_free`].
#[unsafe(no_mangle)]
pub extern "C" fn pi_embedded_new(call: PiHostCallFn, event: PiHostEventFn, context: *mut c_void) -> *mut PiEmbedded {
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("pi-embedded").build()
    else {
        return std::ptr::null_mut();
    };
    let host = Arc::new(CallbackHost { call, event, context: HostContext(context) });
    let bridge = NativeBridge::new(host, runtime.handle().clone());
    Box::into_raw(Box::new(PiEmbedded { runtime: Some(runtime), bridge }))
}

/// Run a command; its outcome arrives as a `command.result` call with the same `id`.
/// # Safety
/// `handle` is live or null; strings are null or NUL-terminated. Null arguments make it a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_embedded_command(
    handle: *mut PiEmbedded,
    id: *const c_char,
    method: *const c_char,
    payload_json: *const c_char,
) {
    // SAFETY: the caller passes a live handle or null.
    let Some(handle) = (unsafe { handle.as_ref() }) else { return };
    // SAFETY: string arguments follow the documented contract.
    let (id, method, payload) = unsafe { (read(id), read(method), read(payload_json)) };
    if let (Some(id), Some(method)) = (id, method) {
        handle.bridge.command(&id, &method, payload.as_deref().unwrap_or("{}"));
    }
}

/// Answer a core call: a non-null `error` rejects it, otherwise `result_json` is its value.
/// # Safety
/// Same contract as [`pi_embedded_command`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_embedded_receive(
    handle: *mut PiEmbedded,
    id: *const c_char,
    result_json: *const c_char,
    error: *const c_char,
) {
    // SAFETY: the caller passes a live handle or null.
    let Some(handle) = (unsafe { handle.as_ref() }) else { return };
    // SAFETY: string arguments follow the documented contract.
    let (id, result, error) = unsafe { (read(id), read(result_json), read(error)) };
    if let Some(id) = id {
        handle.bridge.receive(&id, result.as_deref().unwrap_or("null"), error.as_deref());
    }
}

/// Stop the runtime and release the handle. Do not call it from a host callback.
/// # Safety
/// `handle` comes from [`pi_embedded_new`] and is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_embedded_free(handle: *mut PiEmbedded) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the pointer came from `Box::into_raw` in `pi_embedded_new` and is freed once.
    let mut handle = unsafe { Box::from_raw(handle) };
    if let Some(runtime) = handle.runtime.take() {
        runtime.shutdown_background();
    }
}
