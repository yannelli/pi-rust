//! Helpers for the chord2 (host2) tests: golden fixtures generated from upstream TypeScript, schema builders
//! that write what TypeBox's `Type.*` builders produce, and the runtime tests' hosts, logs, emitters and latches.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use pi_chord::host2::declare::{EventSource, Subscribable};
use pi_chord::host2::{
    ChordErrorCode, Contract, Host, HostOptions, LocalDef, Member as ContractMember, StateEvent, create_host, hash64,
    local,
};
use pi_chord::{ChordError, Json, Unsubscribe, json};

/// A golden fixture from `tests/fixtures/chord2` (see `xtask/chord2-fixtures`).
pub fn fixture(name: &str) -> Json {
    let path = format!("{}/tests/fixtures/chord2/{name}", env!("CARGO_MANIFEST_DIR"));
    Json::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

pub fn text(value: &Json) -> &str {
    value.as_str().unwrap_or_else(|| panic!("not a string: {value}"))
}

/// Compares a text with a fixture entry: the text itself, or `{ length, hash }` for a long one.
pub fn assert_digest(actual: &str, expected: &Json, label: &str) {
    match expected {
        Json::String(expected) => assert_eq!(actual, &**expected, "{label}"),
        digest => {
            let length = actual.encode_utf16().count() as f64;
            assert_eq!(Some(length), digest.get("length").and_then(Json::as_f64), "{label}: length");
            assert_eq!(hash64(actual), text(&digest["hash"]), "{label}: hash");
        }
    }
}

pub fn string() -> Json {
    json!({"type": "string"})
}

pub fn number() -> Json {
    json!({"type": "number"})
}

pub fn integer() -> Json {
    json!({"type": "integer"})
}

pub fn boolean() -> Json {
    json!({"type": "boolean"})
}

pub fn unknown() -> Json {
    json!({})
}

pub fn void() -> Json {
    json!({"type": "void"})
}

pub fn literal(value: &str) -> Json {
    json!({"type": "string", "const": value})
}

pub fn array(items: Json) -> Json {
    json!({"type": "array", "items": items.to_serde()})
}

pub fn union(branches: Vec<Json>) -> Json {
    Json::from(serde_json::json!({"anyOf": branches.iter().map(Json::to_serde).collect::<Vec<_>>()}))
}

pub fn function(parameters: Vec<Json>, returns: Json) -> Json {
    let parameters: Vec<_> = parameters.iter().map(Json::to_serde).collect();
    json!({"type": "function", "parameters": parameters, "returnType": returns.to_serde()})
}

/// `Type.Object`: required properties first in `required`, optional ones (`Some(true)`) left out of it.
pub fn object(properties: &[(&str, Json, bool)]) -> Json {
    let mut schema = serde_json::Map::new();
    schema.insert("type".into(), "object".into());
    let required: Vec<_> = properties.iter().filter(|(_, _, optional)| !optional).map(|(name, ..)| *name).collect();
    if !required.is_empty() {
        schema.insert("required".into(), required.into());
    }
    let properties = properties.iter().map(|(name, schema, _)| (name.to_string(), schema.to_serde()));
    schema.insert("properties".into(), serde_json::Value::Object(properties.collect()));
    Json::from(serde_json::Value::Object(schema))
}

/// A required property.
pub fn req(name: &str, schema: Json) -> (&str, Json, bool) {
    (name, schema, false)
}

/// An optional property.
pub fn opt(name: &str, schema: Json) -> (&str, Json, bool) {
    (name, schema, true)
}

/// A schema with extra keywords merged in (`Type.String({ minLength: 1 })`).
pub fn with(schema: Json, extra: serde_json::Value) -> Json {
    let mut schema = schema.to_serde();
    if let (Some(target), serde_json::Value::Object(extra)) = (schema.as_object_mut(), extra) {
        target.extend(extra);
    }
    Json::from(schema)
}

pub mod cluster;
pub mod net;
pub mod reload;
pub mod tutorial;

/// Serializes the runtime tests of one binary: the publish queue is process-wide.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.lock().await
}

/// Lets spawned work run (upstream `setImmediate`).
pub async fn tick() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

/// Ticks until `check` holds, at most 200 times.
pub async fn until(check: impl Fn() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tick().await;
    }
    assert!(check(), "condition not reached");
}

pub async fn sleep_ms(ms: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

/// A shared list that closures push to.
#[derive(Clone)]
pub struct Log<T = String>(Arc<Mutex<Vec<T>>>);

impl<T> Default for Log<T> {
    fn default() -> Self {
        Self(Arc::default())
    }
}

impl<T: Clone> Log<T> {
    pub fn push(&self, value: impl Into<T>) {
        self.0.lock().unwrap().push(value.into());
    }

    /// Pushes `value`; the result is a settled `Ok(())` for a resource's `open` or `close`.
    pub fn push_ready(&self, value: impl Into<T>) -> std::future::Ready<pi_chord::ChordResult<()>> {
        self.push(value);
        std::future::ready(Ok(()))
    }

    pub fn all(&self) -> Vec<T> {
        self.0.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// Host options with the host's own identity `{kind: "host"}` and errors collected in `errors`.
pub fn quiet_options(errors: &Log<ChordError>) -> HostOptions {
    let errors = errors.clone();
    let on_error: Arc<dyn Fn(ChordError) + Send + Sync> = Arc::new(move |error| errors.push(error));
    HostOptions { principal: json!({"kind": "host"}), on_error: Some(on_error), ..HostOptions::default() }
}

pub fn quiet_with(errors: &Log<ChordError>) -> Host {
    create_host(quiet_options(errors))
}

pub fn quiet() -> Host {
    quiet_with(&Log::default())
}

/// A local type of version 1.
pub fn local_type(name: &str, input: Json, members: Vec<(&str, ContractMember)>) -> Contract {
    let members = members.into_iter().map(|(name, member)| (name.to_string(), member)).collect();
    local(name, &LocalDef { version: 1, input, members }).unwrap()
}

pub fn code_of(error: &ChordError) -> Option<&'static str> {
    error.code().map(ChordErrorCode::as_str)
}

/// Whether `error` is a chord error of `code` (upstream `isChordError(error, code)`).
pub fn is(error: &ChordError, code: &str) -> bool {
    code_of(error) == Some(code)
}

type EmitterListener = Arc<dyn Fn(&Json) + Send + Sync>;
/// The last listener id and the listeners.
type Listeners = (u64, Vec<(u64, EmitterListener)>);

/// An event source with a listener count (upstream test `emitter()`). Clones share their listeners.
#[derive(Clone, Default)]
pub struct Emitter(Arc<Mutex<Listeners>>);

impl Emitter {
    pub fn emit(&self, event: Json) {
        let listeners: Vec<EmitterListener> = self.0.lock().unwrap().1.iter().map(|(_, l)| l.clone()).collect();
        for listener in listeners {
            listener(&event);
        }
    }

    pub fn size(&self) -> usize {
        self.0.lock().unwrap().1.len()
    }

    pub fn source(&self) -> EventSource {
        Arc::new(self.clone())
    }
}

impl Subscribable<Json> for Emitter {
    fn subscribe(&self, listener: Box<dyn Fn(&Json) + Send + Sync>) -> Unsubscribe {
        let id = {
            let mut state = self.0.lock().unwrap();
            state.0 += 1;
            let id = state.0;
            state.1.push((id, Arc::from(listener)));
            id
        };
        let inner = Arc::downgrade(&self.0);
        Unsubscribe::new(move || {
            if let Some(inner) = inner.upgrade() {
                inner.lock().unwrap().1.retain(|(other, _)| *other != id);
            }
        })
    }
}

/// A state event as upstream's object: `{type, value}`, with `ops` in tuple form for an ops event.
pub fn event_json(event: &StateEvent) -> Json {
    match event {
        StateEvent::Ops { ops, value } => {
            let ops: Vec<Json> = ops.iter().map(|op| op.to_json()).collect();
            json!({"type": "ops", "ops": ops, "value": value})
        }
        other => json!({"type": other.kind(), "value": other.value()}),
    }
}

/// A value that is set once and awaited (upstream test `latch()`).
#[derive(Clone)]
pub struct Latch(Arc<tokio::sync::watch::Sender<Option<Json>>>);

impl Default for Latch {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::watch::channel(None).0))
    }
}

impl Latch {
    pub fn open(&self, value: Json) {
        self.0.send_replace(Some(value));
    }

    pub fn wait(&self) -> impl std::future::Future<Output = Json> + Send + use<> {
        let mut receiver = self.0.subscribe();
        async move {
            let value = receiver.wait_for(Option::is_some).await.ok().and_then(|value| value.clone());
            value.unwrap_or_default()
        }
    }
}
