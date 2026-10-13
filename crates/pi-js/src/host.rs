//! Values and host functions shared between Rust and JavaScript.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::error::JsError;
use crate::runtime::JsRef;

/// A value crossing the bridge. Plain data travels as JSON (with `JSON.stringify` rules),
/// `Uint8Array`/`ArrayBuffer` as bytes, and a top-level function becomes a reference.
#[derive(Clone, Debug, Default)]
pub enum HostValue {
    #[default]
    Undefined,
    Json(serde_json::Value),
    Bytes(Vec<u8>),
    Ref(JsRef),
}

impl HostValue {
    pub fn as_json(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Json(value) => Some(value),
            _ => None,
        }
    }

    /// The JSON form: `undefined` and references become `null`, bytes an array of numbers.
    pub fn into_json(self) -> serde_json::Value {
        match self {
            Self::Json(value) => value,
            Self::Bytes(bytes) => bytes.into_iter().map(serde_json::Value::from).collect(),
            Self::Undefined | Self::Ref(_) => serde_json::Value::Null,
        }
    }

    pub fn as_js_ref(&self) -> Option<&JsRef> {
        match self {
            Self::Ref(reference) => Some(reference),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        self.as_json().and_then(serde_json::Value::as_str)
    }

    pub fn is_undefined(&self) -> bool {
        matches!(self, Self::Undefined)
    }
}

impl From<serde_json::Value> for HostValue {
    fn from(value: serde_json::Value) -> Self {
        Self::Json(value)
    }
}

impl From<JsRef> for HostValue {
    fn from(value: JsRef) -> Self {
        Self::Ref(value)
    }
}

impl From<Vec<u8>> for HostValue {
    fn from(value: Vec<u8>) -> Self {
        Self::Bytes(value)
    }
}

impl From<&str> for HostValue {
    fn from(value: &str) -> Self {
        Self::Json(value.into())
    }
}

impl From<String> for HostValue {
    fn from(value: String) -> Self {
        Self::Json(value.into())
    }
}

type SyncFn = dyn Fn(Vec<HostValue>) -> Result<HostValue, JsError> + Send + Sync;
type AsyncFn = dyn Fn(Vec<HostValue>) -> BoxFuture<'static, Result<HostValue, JsError>> + Send + Sync;

/// A Rust function JavaScript calls through the `pi:host` module.
/// Sync functions run on the JavaScript thread; async ones run on tokio and settle a promise.
#[derive(Clone)]
pub enum HostFunction {
    Sync(Arc<SyncFn>),
    Async(Arc<AsyncFn>),
}

impl HostFunction {
    pub fn sync(function: impl Fn(Vec<HostValue>) -> Result<HostValue, JsError> + Send + Sync + 'static) -> Self {
        Self::Sync(Arc::new(function))
    }

    pub fn asynchronous<F, Fut>(function: F) -> Self
    where
        F: Fn(Vec<HostValue>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<HostValue, JsError>> + Send + 'static,
    {
        Self::Async(Arc::new(move |args| Box::pin(function(args))))
    }
}

impl std::fmt::Debug for HostFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Sync(_) => "HostFunction::Sync",
            Self::Async(_) => "HostFunction::Async",
        })
    }
}

/// Named host functions, registered before or after the runtime starts.
#[derive(Clone, Debug, Default)]
pub struct HostFunctions {
    functions: HashMap<String, HostFunction>,
}

impl HostFunctions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl Into<String>, function: HostFunction) -> &mut Self {
        self.functions.insert(name.into(), function);
        self
    }

    pub fn get(&self, name: &str) -> Option<&HostFunction> {
        self.functions.get(name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.functions.keys().cloned().collect();
        names.sort();
        names
    }
}
