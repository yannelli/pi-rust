//! Conversions between JavaScript values and Rust values and errors.

use std::cell::RefCell;
use std::rc::Rc;

use rquickjs::function::Opt;
use rquickjs::{Coerced, Constructor, Ctx, Function, Object, Persistent, Promise, TypedArray, Value};

use super::state::state;
use super::{JsHandle, JsRef};
use crate::error::JsError;
use crate::host::HostValue;
use crate::transpile::remap_stack;

const ERROR_CONSTRUCTORS: &[&str] =
    &["TypeError", "RangeError", "SyntaxError", "ReferenceError", "EvalError", "URIError"];

/// A JavaScript error object for a Rust error.
pub(crate) fn error_value<'js>(ctx: &Ctx<'js>, error: &JsError) -> rquickjs::Result<Value<'js>> {
    let constructor_name =
        if ERROR_CONSTRUCTORS.contains(&error.name.as_str()) { error.name.as_str() } else { "Error" };
    let constructor: Constructor = ctx.globals().get(constructor_name)?;
    let object: Object = constructor.construct((error.message.clone(),))?;
    if error.name != constructor_name && !error.name.is_empty() {
        object.set("name", error.name.clone())?;
    }
    if let Some(code) = &error.code {
        object.set("code", code.clone())?;
    }
    if let Some(stack) = &error.stack {
        object.set("stack", stack.clone())?;
    }
    for (key, value) in error.data.iter().flat_map(|data| data.iter()) {
        object.set(key.as_str(), from_json(ctx, value)?)?;
    }
    Ok(object.into_value())
}

/// Throw a Rust error into JavaScript.
pub(crate) fn throw(ctx: &Ctx<'_>, error: &JsError) -> rquickjs::Error {
    match error_value(ctx, error) {
        Ok(value) => ctx.throw(value),
        Err(failure) => failure,
    }
}

/// The JavaScript exception behind an rquickjs error, as a Rust error.
pub(crate) fn caught(ctx: &Ctx<'_>, error: rquickjs::Error) -> JsError {
    match error {
        rquickjs::Error::Exception => error_from_value(ctx, ctx.catch()),
        other => JsError::new(other.to_string()),
    }
}

fn string_property(object: &Object<'_>, key: &str) -> Option<String> {
    let value: Value = object.get(key).ok()?;
    if let Some(text) = value.as_string() {
        return text.to_string().ok();
    }
    value.as_number().map(|number| number.to_string())
}

/// Read a thrown value: `name`, `message`, remapped `stack`, `code`, and other own properties.
pub(crate) fn error_from_value<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> JsError {
    let Some(object) = value.as_object().filter(|_| !value.is_function()) else {
        let message = value.get::<Coerced<String>>().map(|text| text.0).unwrap_or_default();
        return JsError::with_name("", message);
    };
    let mut error = JsError::with_name(
        string_property(object, "name").unwrap_or_else(|| "Error".into()),
        string_property(object, "message").unwrap_or_default(),
    );
    error.stack = string_property(object, "stack").map(|stack| remap_stack(&stack)).filter(|stack| !stack.is_empty());
    error.code = string_property(object, "code");
    if let Ok(serde_json::Value::Object(mut data)) = to_json(ctx, value.clone()) {
        for key in ["name", "message", "stack", "code"] {
            data.remove(key);
        }
        if !data.is_empty() {
            error.data = Some(Box::new(data));
        }
    }
    error
}

pub(crate) fn to_json<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Result<serde_json::Value, JsError> {
    match ctx.json_stringify(value) {
        Ok(Some(text)) => {
            let text = text.to_string().map_err(|error| caught(ctx, error))?;
            Ok(serde_json::from_str(&text)?)
        }
        Ok(None) => Ok(serde_json::Value::Null),
        Err(error) => Err(caught(ctx, error)),
    }
}

pub(crate) fn from_json<'js>(ctx: &Ctx<'js>, value: &serde_json::Value) -> rquickjs::Result<Value<'js>> {
    match value {
        serde_json::Value::Null => Ok(Value::new_null(ctx.clone())),
        serde_json::Value::Bool(flag) => Ok(Value::new_bool(ctx.clone(), *flag)),
        serde_json::Value::String(text) => rquickjs::String::from_str(ctx.clone(), text).map(|text| text.into_value()),
        other => ctx.json_parse(other.to_string()),
    }
}

/// Bytes from a JavaScript string whose characters are all below 256.
pub(crate) fn latin1_to_bytes(text: &str) -> Vec<u8> {
    text.chars().map(|character| character as u32 as u8).collect()
}

pub(crate) fn bytes_to_js<'js>(ctx: &Ctx<'js>, bytes: Vec<u8>) -> rquickjs::Result<Value<'js>> {
    TypedArray::<u8>::new(ctx.clone(), bytes).map(|array| array.into_value())
}

/// Bytes of a `Uint8Array`, other typed array, `DataView`, or `ArrayBuffer`; `None` otherwise.
pub(crate) fn js_bytes<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> rquickjs::Result<Option<Vec<u8>>> {
    if !value.is_object() {
        return Ok(None);
    }
    let to_latin1: Function = state().internal(ctx)?.get("bytesToLatin1")?;
    let text: Option<String> = to_latin1.call((value.clone(),))?;
    Ok(text.map(|text| latin1_to_bytes(&text)))
}

pub(crate) fn persist<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> JsRef {
    let state = state();
    let id = state.next_id();
    state.refs.borrow_mut().insert(id, Persistent::save(ctx, value));
    JsRef::new(id, JsHandle::from_state(&state))
}

pub(crate) fn restore<'js>(ctx: &Ctx<'js>, reference: &JsRef) -> Result<Value<'js>, JsError> {
    let state = state();
    if !reference.handle().belongs_to(&state.shared) {
        return Err(JsError::new("JsRef belongs to another JavaScript runtime"));
    }
    let stored = state.refs.borrow().get(&reference.id()).cloned();
    let stored = stored.ok_or_else(|| JsError::new("JsRef was released"))?;
    stored.restore(ctx).map_err(|error| caught(ctx, error))
}

pub(crate) fn to_host<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Result<HostValue, JsError> {
    if value.is_undefined() {
        return Ok(HostValue::Undefined);
    }
    if value.is_function() {
        return Ok(HostValue::Ref(persist(ctx, value)));
    }
    if let Some(bytes) = js_bytes(ctx, &value).map_err(|error| caught(ctx, error))? {
        return Ok(HostValue::Bytes(bytes));
    }
    match ctx.json_stringify(value) {
        Ok(None) => Ok(HostValue::Undefined),
        Ok(Some(text)) => {
            let text = text.to_string().map_err(|error| caught(ctx, error))?;
            Ok(HostValue::Json(serde_json::from_str(&text)?))
        }
        Err(error) => Err(caught(ctx, error)),
    }
}

pub(crate) fn from_host<'js>(ctx: &Ctx<'js>, value: HostValue) -> Result<Value<'js>, JsError> {
    match value {
        HostValue::Undefined => Ok(Value::new_undefined(ctx.clone())),
        HostValue::Json(json) => from_json(ctx, &json).map_err(|error| caught(ctx, error)),
        HostValue::Bytes(bytes) => bytes_to_js(ctx, bytes).map_err(|error| caught(ctx, error)),
        HostValue::Ref(reference) => restore(ctx, &reference),
    }
}

type Settle = Box<dyn for<'a> FnOnce(&Ctx<'a>, Result<Value<'a>, JsError>)>;

/// Pin a closure to the higher-ranked signature [`when_settled`] expects.
pub(crate) fn settled<F>(f: F) -> F
where
    F: for<'a> FnOnce(&Ctx<'a>, Result<Value<'a>, JsError>) + 'static,
{
    f
}

/// Call `done` with the value, or with the settled result once it is a promise.
pub(crate) fn when_settled<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
    done: impl for<'a> FnOnce(&Ctx<'a>, Result<Value<'a>, JsError>) + 'static,
) -> rquickjs::Result<()> {
    let Some(promise) = value.as_promise().cloned() else {
        done(ctx, Ok(value));
        return Ok(());
    };
    let slot: Rc<RefCell<Option<Settle>>> = Rc::new(RefCell::new(Some(Box::new(done))));
    let ok_slot = slot.clone();
    let on_ok = Function::new(ctx.clone(), move |ctx: Ctx<'js>, value: Opt<Value<'js>>| {
        if let Some(done) = ok_slot.borrow_mut().take() {
            done(&ctx, Ok(value.0.unwrap_or_else(|| Value::new_undefined(ctx.clone()))));
        }
    })?;
    let on_error = Function::new(ctx.clone(), move |ctx: Ctx<'js>, reason: Opt<Value<'js>>| {
        if let Some(done) = slot.borrow_mut().take() {
            let reason = reason.0.unwrap_or_else(|| Value::new_undefined(ctx.clone()));
            done(&ctx, Err(error_from_value(&ctx, reason)));
        }
    })?;
    promise_then(&promise, on_ok, on_error)
}

fn promise_then<'js>(promise: &Promise<'js>, on_ok: Function<'js>, on_error: Function<'js>) -> rquickjs::Result<()> {
    let then: Function = promise.get("then")?;
    then.call::<_, Value>((rquickjs::function::This(promise.clone()), on_ok, on_error))?;
    Ok(())
}
