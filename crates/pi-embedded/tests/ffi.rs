//! The C ABI round trip: commands in, host calls and command results out, answers back in.
// Calling the C ABI from Rust needs `unsafe`; the crate lint is `deny`, so this test allows it.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use pi_embedded::ffi::{pi_embedded_command, pi_embedded_free, pi_embedded_new, pi_embedded_receive};
use serde_json::{Value, json};

type Call = (String, String, String);

fn text(value: *const c_char) -> String {
    // SAFETY: the bridge passes NUL-terminated strings valid for the callback.
    unsafe { CStr::from_ptr(value) }.to_string_lossy().into_owned()
}

extern "C" fn on_call(context: *mut c_void, id: *const c_char, method: *const c_char, payload: *const c_char) {
    // SAFETY: `context` is the `Mutex<Sender<Call>>` leaked by the test for the bridge lifetime.
    let sender = unsafe { &*(context as *const Mutex<Sender<Call>>) };
    let _ = sender.lock().unwrap().send((text(id), text(method), text(payload)));
}

extern "C" fn on_event(_: *mut c_void, _: *const c_char) {}

fn next(calls: &Receiver<Call>) -> Call {
    calls.recv_timeout(Duration::from_secs(10)).expect("a host call")
}

fn c(value: &str) -> CString {
    CString::new(value).unwrap()
}

#[test]
fn c_abi_runs_commands_and_answers_host_calls() {
    let (sender, calls) = channel::<Call>();
    let context = Box::into_raw(Box::new(Mutex::new(sender))) as *mut c_void;
    let handle = pi_embedded_new(on_call, on_event, context);
    assert!(!handle.is_null());
    let command = |id: &str, method: &str, payload: Value| {
        let (id, method, payload) = (c(id), c(method), c(&payload.to_string()));
        // SAFETY: a live handle and NUL-terminated strings.
        unsafe { pi_embedded_command(handle, id.as_ptr(), method.as_ptr(), payload.as_ptr()) };
    };
    let result = |call: Call| -> Value {
        assert_eq!(call.1, "command.result");
        serde_json::from_str(&call.2).unwrap()
    };

    command("c1", "configure", json!({"capabilities": []}));
    assert_eq!(result(next(&calls)), json!({"value": null}));
    command("c2", "snapshot", json!({"id": "missing"}));
    assert_eq!(result(next(&calls)), json!({"error": "Session is not open"}));

    command("c3", "list", json!({}));
    let (id, method, payload) = next(&calls);
    assert_eq!((method.as_str(), payload.as_str()), ("store.list", "{}"));
    let (id, answer) = (c(&id), c(r#"[{"id":"saved","status":"idle","revision":3}]"#));
    // SAFETY: a live handle; a null error means success.
    unsafe { pi_embedded_receive(handle, id.as_ptr(), answer.as_ptr(), std::ptr::null()) };
    assert_eq!(result(next(&calls)), json!({"value": [{"id": "saved", "status": "idle", "revision": 3}]}));

    command("c4", "list", json!({}));
    let (id, _, _) = next(&calls);
    let (id, error) = (c(&id), c("disk offline"));
    // SAFETY: a live handle and NUL-terminated strings.
    unsafe { pi_embedded_receive(handle, id.as_ptr(), std::ptr::null(), error.as_ptr()) };
    assert_eq!(result(next(&calls)), json!({"error": "disk offline"}));

    // SAFETY: the handle is not used again; the context outlives the stopped runtime.
    unsafe { pi_embedded_free(handle) };
    // SAFETY: reclaims the context leaked above after the bridge stopped.
    drop(unsafe { Box::from_raw(context as *mut Mutex<Sender<Call>>) });
}
