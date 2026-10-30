//! [`HostValue`] on the wire: `["u"]`, `["j", json]`, `["b", base64]`, or `["r", ref id]`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::error::JsError;
use crate::host::HostValue;
use crate::runtime::{JsHandle, JsRef};

pub(crate) fn encode(handle: &JsHandle, value: &HostValue) -> Result<Value, JsError> {
    Ok(match value {
        HostValue::Undefined => json!(["u"]),
        HostValue::Json(value) => json!(["j", value]),
        HostValue::Bytes(bytes) => json!(["b", STANDARD.encode(bytes)]),
        HostValue::Ref(reference) => {
            if !reference.handle().same_runtime(handle) {
                return Err(JsError::new("JsRef belongs to another JavaScript runtime"));
            }
            json!(["r", reference.id()])
        }
    })
}

pub(crate) fn encode_all(handle: &JsHandle, values: &[HostValue]) -> Result<Value, JsError> {
    values.iter().map(|value| encode(handle, value)).collect::<Result<Vec<_>, _>>().map(Value::Array)
}

pub(crate) fn decode(handle: &JsHandle, wire: &Value) -> Result<HostValue, JsError> {
    let malformed = || JsError::new(format!("malformed value from Node: {wire}"));
    let tag = wire.get(0).and_then(Value::as_str).ok_or_else(malformed)?;
    Ok(match tag {
        "u" => HostValue::Undefined,
        "j" => HostValue::Json(wire.get(1).cloned().unwrap_or(Value::Null)),
        "b" => {
            let text = wire.get(1).and_then(Value::as_str).ok_or_else(malformed)?;
            HostValue::Bytes(STANDARD.decode(text).map_err(|_| malformed())?)
        }
        "r" => HostValue::Ref(JsRef::new(wire.get(1).and_then(Value::as_u64).ok_or_else(malformed)?, handle.clone())),
        _ => return Err(malformed()),
    })
}

pub(crate) fn decode_all(handle: &JsHandle, wire: Option<&Value>) -> Result<Vec<HostValue>, JsError> {
    let items = wire.and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    items.iter().map(|item| decode(handle, item)).collect()
}

/// A JavaScript error as Node sent it.
pub(crate) fn error(value: &Value) -> JsError {
    serde_json::from_value(value.clone()).unwrap_or_else(|_| JsError::new(value.to_string()))
}

pub(crate) fn error_json(error: &JsError) -> Value {
    serde_json::to_value(error).unwrap_or_else(|_| json!({ "name": "Error", "message": error.message }))
}
