//! Requests [`JsHandle`] and [`JsRef`] send to the Node worker, and their results.

use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::client::{Waiter, shut_down};
use super::wire;
use crate::error::JsError;
use crate::host::HostValue;
use crate::runtime::{JsHandle, JsRef};

/// How the worker returns a result.
#[derive(Clone, Copy)]
pub(crate) enum Want {
    Value,
    Ref,
    OptRef,
}

impl Want {
    fn name(self) -> &'static str {
        match self {
            Self::Value => "value",
            Self::Ref => "ref",
            Self::OptRef => "optRef",
        }
    }
}

pub(crate) fn frame(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

pub(crate) fn import_frame(specifier: &str, default: bool) -> Map<String, Value> {
    frame(json!({ "op": "import", "specifier": specifier, "default": default, "want": "ref" }))
}

pub(crate) fn call_frame(
    target: &JsRef,
    method: Option<&str>,
    args: &[HostValue],
    want: Want,
) -> Result<Map<String, Value>, JsError> {
    let args = wire::encode_all(target.handle(), args)?;
    let mut map = frame(json!({ "op": "call", "target": target.id(), "args": args, "want": want.name() }));
    if let Some(method) = method {
        map.insert("method".into(), json!(method));
    }
    Ok(map)
}

pub(crate) fn get_frame(target: &JsRef, key: &str, want: Want) -> Map<String, Value> {
    frame(json!({ "op": "get", "target": target.id(), "key": key, "want": want.name() }))
}

pub(crate) fn set_frame(target: &JsRef, key: &str, value: &HostValue) -> Result<Map<String, Value>, JsError> {
    let value = wire::encode(target.handle(), value)?;
    Ok(frame(json!({ "op": "set", "target": target.id(), "key": key, "value": value, "want": "unit" })))
}

pub(crate) fn json_frame(target: &JsRef) -> Map<String, Value> {
    frame(json!({ "op": "json", "target": target.id(), "want": "json" }))
}

pub(crate) async fn ask(handle: &JsHandle, frame: Result<Map<String, Value>, JsError>) -> Result<HostValue, JsError> {
    let client = handle.node_client().ok_or_else(shut_down)?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    client.send_request(&handle.shared().alive, frame?, Waiter::Async(sender))?;
    receiver.await.map_err(|_| client.exited())?
}

/// [`ask`] for synchronous callers; fails on the bridge thread, which must read the reply.
pub(crate) fn blocking_ask(
    handle: &JsHandle,
    frame: Result<Map<String, Value>, JsError>,
    timeout: Option<Duration>,
) -> Result<HostValue, JsError> {
    handle.refuse_on_js_thread()?;
    let client = handle.node_client().ok_or_else(shut_down)?;
    let (sender, receiver) = mpsc::channel();
    client.send_request(&handle.shared().alive, frame?, Waiter::Blocking(sender))?;
    match timeout {
        None => receiver.recv().map_err(|_| client.exited())?,
        Some(timeout) => match receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(JsError::new("JavaScript call timed out")),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(client.exited()),
        },
    }
}

pub(crate) fn as_ref(value: HostValue) -> Result<JsRef, JsError> {
    match value {
        HostValue::Ref(reference) => Ok(reference),
        _ => Err(JsError::new("the Node worker returned a value where a reference was expected")),
    }
}

pub(crate) fn as_opt_ref(value: HostValue) -> Result<Option<JsRef>, JsError> {
    match value {
        HostValue::Undefined => Ok(None),
        other => as_ref(other).map(Some),
    }
}
