//! The Rust side of the embedded `@earendil-works/chord` module: tables of host objects and
//! the `chord` host functions that JavaScript facets call.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use pi_js::{HostValue, JsError, JsHandle, JsRef, JsRuntime};
use serde_json::Value;

use super::JsObject;
use crate::context::{AbortSignal, BACKGROUND_CONTEXT, Context};
use crate::error::{ChordError, ChordResult, RemoteServiceErrorCode, report_unhandled};
use crate::facets::{FacetEnvironment, LocalServiceSpawner, ServiceSpawner};
use crate::json::Json;
use crate::services::{LocalHandle, ServiceHandle, ServiceImpl, ServiceRef, define_local_service, observe_handler};
use crate::state::{DeliveryKind, MutableReplicatedState, StateListener, replicated_state};
use crate::sync::{Unsubscribe, lock, spawn_eager};

enum Spawner {
    Remote(ServiceSpawner),
    Local(LocalServiceSpawner<JsObject>),
}

#[derive(Default)]
struct Tables {
    envs: HashMap<u64, FacetEnvironment>,
    handles: HashMap<u64, ServiceHandle>,
    locals: HashMap<u64, LocalHandle<JsObject>>,
    states: HashMap<u64, MutableReplicatedState>,
    spawners: HashMap<u64, Spawner>,
    closers: HashMap<u64, Unsubscribe>,
}

pub(crate) struct Bridge {
    pub handle: JsHandle,
    pub api: OnceLock<JsRef>,
    tables: Mutex<Tables>,
    next: AtomicU64,
    runtime: Mutex<Option<JsRuntime>>,
}

impl Drop for Bridge {
    /// Stopping a runtime joins its thread, which may be the current one; stop it elsewhere.
    fn drop(&mut self) {
        if let Some(runtime) = lock(&self.runtime).take() {
            std::thread::spawn(move || drop(runtime));
        }
    }
}

fn upgrade(weak: &Weak<Bridge>) -> ChordResult<Arc<Bridge>> {
    weak.upgrade().ok_or_else(|| ChordError::error("The JavaScript facet runtime is closed"))
}

pub(crate) fn js_error(error: ChordError) -> JsError {
    let message = error.to_string();
    match error.remote_code() {
        Some(code) => JsError::new(message).with_code(code.as_str()),
        None => JsError::new(message),
    }
}

pub(crate) fn chord_error(error: JsError) -> ChordError {
    let code = error.code.as_deref().and_then(RemoteServiceErrorCode::parse);
    match code {
        Some(code) => ChordError::remote(code, error.message),
        None if error.name == "TypeError" => ChordError::type_error(error.message),
        None => ChordError::error(error.message),
    }
}

/// A JavaScript result as an optional JSON value: `undefined` is `None`.
pub(crate) fn json_result(value: HostValue) -> Option<Json> {
    match value {
        HostValue::Undefined | HostValue::Ref(_) => None,
        other => Some(Json::from(&other.into_json())),
    }
}

fn host_json(value: Option<Json>) -> HostValue {
    value.map_or(HostValue::Undefined, |value| HostValue::Json(value.to_serde()))
}

struct Args(Vec<HostValue>);

impl Args {
    fn json(&self, index: usize) -> Value {
        self.0.get(index).and_then(HostValue::as_json).cloned().unwrap_or(Value::Null)
    }

    fn id(&self, index: usize) -> Result<u64, JsError> {
        self.json(index).as_u64().ok_or_else(|| JsError::type_error("chord bridge expected an object ID"))
    }

    fn text(&self, index: usize) -> Result<String, JsError> {
        self.json(index)
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| JsError::type_error("chord bridge expected a string"))
    }

    fn token(&self, index: usize) -> Result<(String, bool), JsError> {
        let token = self.json(index);
        let id = token.get("id").and_then(Value::as_str).ok_or_else(|| JsError::type_error("Invalid service token"))?;
        Ok((id.to_string(), token.get("local").and_then(Value::as_bool).unwrap_or(false)))
    }
}

type Listening = Option<(AbortSignal, u64)>;

fn id_value(id: u64) -> HostValue {
    Value::from(id).into()
}

fn view(local: bool, handle: u64) -> HostValue {
    serde_json::json!({"local": local, "handle": handle}).into()
}

impl Bridge {
    pub fn new(handle: JsHandle, runtime: Option<JsRuntime>) -> Arc<Self> {
        Arc::new(Self {
            handle,
            api: OnceLock::new(),
            tables: Mutex::default(),
            next: AtomicU64::new(1),
            runtime: Mutex::new(runtime),
        })
    }

    pub fn api(&self) -> ChordResult<&JsRef> {
        self.api.get().ok_or_else(|| ChordError::error("The embedded chord runtime is not ready"))
    }

    fn id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst)
    }

    fn insert(&self, add: impl FnOnce(&mut Tables, u64)) -> u64 {
        let id = self.id();
        add(&mut lock(&self.tables), id);
        id
    }

    pub fn register_env(&self, env: FacetEnvironment) -> u64 {
        self.insert(|tables, id| drop(tables.envs.insert(id, env)))
    }

    pub fn forget_env(&self, id: u64) {
        lock(&self.tables).envs.remove(&id);
    }

    /// Call a method of the JavaScript runtime object without waiting, from any thread.
    pub fn post(&self, method: &'static str, args: Vec<HostValue>) {
        let Ok(api) = self.api().cloned() else { return };
        spawn_eager(async move {
            if let Err(error) = api.call_method(method, args).await {
                report_unhandled(&chord_error(error));
            }
        });
    }

    /// A JavaScript context ID for `context`; cancelling it aborts the JavaScript signal.
    fn context_id(self: &Arc<Self>, context: &Context) -> (u64, Listening) {
        let Some(signal) = context.abort_signal() else { return (0, None) };
        let id = self.id();
        let weak = Arc::downgrade(self);
        let listener = signal.add_listener(move |reason| {
            if let Some(bridge) = weak.upgrade() {
                bridge.post("abort", vec![id_value(id), reason.to_string().into()]);
            }
        });
        (id, Some((signal, listener)))
    }

    fn release_context(&self, id: u64, listening: Listening) {
        if let Some((signal, listener)) = listening {
            signal.remove_listener(listener);
            self.post("releaseContext", vec![id_value(id)]);
        }
    }

    async fn call_function(&self, function: u64) -> ChordResult<()> {
        let args = vec![id_value(function), Value::Array(Vec::new()).into()];
        self.api()?.call_method("callFunction", args).await.map_err(chord_error)?;
        Ok(())
    }

    pub async fn invoke(
        self: &Arc<Self>,
        object: u64,
        member: &str,
        args: Vec<Json>,
        context: &Context,
    ) -> ChordResult<Option<Json>> {
        let (context_id, listening) = self.context_id(context);
        let args = Value::Array(args.iter().map(Json::to_serde).collect());
        let call = vec![id_value(object), member.into(), args.into(), id_value(context_id)];
        let result = self.api()?.call_method("invoke", call).await.map_err(chord_error);
        self.release_context(context_id, listening);
        result.map(json_result)
    }

    fn listener(self: &Arc<Self>, function: u64) -> StateListener {
        let weak = Arc::downgrade(self);
        StateListener::sync(move |value, _, delivery| {
            let kind = match delivery.kind {
                DeliveryKind::Hydrate => "hydrate",
                DeliveryKind::Update => "update",
            };
            let detail = serde_json::json!({"kind": kind, "sequence": delivery.sequence});
            let args = Value::Array(vec![value.to_serde(), Value::Null, detail]);
            upgrade(&weak)?.post("callFunction", vec![id_value(function), args.into()]);
            Ok(())
        })
    }

    fn service_impl(self: &Arc<Self>, object: u64, description: &Value) -> Result<ServiceImpl, JsError> {
        let mut implementation = ServiceImpl::new();
        for member in description.as_array().into_iter().flatten() {
            let name = member.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            if member.get("kind").and_then(Value::as_str) == Some("method") {
                let (weak, member) = (Arc::downgrade(self), name.clone());
                implementation = implementation.method(name, move |args, context| {
                    let (weak, member) = (weak.clone(), member.clone());
                    async move { upgrade(&weak)?.invoke(object, &member, args, &context).await }
                });
                continue;
            }
            let id = member.get("state").and_then(Value::as_u64).unwrap_or_default();
            let state = lock(&self.tables).states.get(&id).cloned();
            let state = state.ok_or_else(|| JsError::type_error(format!("Unknown replicated state {id}")))?;
            implementation = implementation.state(name, &state);
        }
        Ok(implementation)
    }

    fn closer(&self, closer: Unsubscribe) -> HostValue {
        id_value(self.insert(|tables, id| drop(tables.closers.insert(id, closer))))
    }

    fn environment(self: &Arc<Self>, args: &Args) -> Result<HostValue, JsError> {
        let env = lock(&self.tables).envs.get(&args.id(1)?).cloned();
        let env = env.ok_or_else(|| JsError::new("Facet environment is no longer available"))?;
        let operation = args.text(2)?;
        let local_service = |id: &str| define_local_service::<JsObject>(id).map_err(js_error);
        let result = match operation.as_str() {
            "use" => {
                let (id, local) = args.token(3)?;
                if local {
                    let handle = env.use_local(&local_service(&id)?).map_err(js_error)?;
                    return Ok(view(true, self.insert(|tables, id| drop(tables.locals.insert(id, handle)))));
                }
                let handle = env.use_service(&ServiceRef::remote(id)).map_err(js_error)?;
                return Ok(view(false, self.insert(|tables, id| drop(tables.handles.insert(id, handle)))));
            }
            "observe" => {
                let ((id, local), handler) = (args.token(3)?, args.id(4)?);
                let weak = Arc::downgrade(self);
                if local {
                    env.observe_local(&local_service(&id)?, move |handle, context| {
                        let weak = weak.clone();
                        async move {
                            let bridge = upgrade(&weak)?;
                            let stored = bridge.insert(|tables, id| drop(tables.locals.insert(id, handle)));
                            bridge.observe(handler, true, stored, context).await
                        }
                    })
                } else {
                    let handler = observe_handler(move |handle, context| {
                        let weak = weak.clone();
                        async move {
                            let bridge = upgrade(&weak)?;
                            let stored = bridge.insert(|tables, id| drop(tables.handles.insert(id, handle)));
                            bridge.observe(handler, false, stored, context).await
                        }
                    });
                    env.observe(&ServiceRef::remote(id), handler)
                }
            }
            "provide" => {
                let ((id, local), object) = (args.token(3)?, args.id(4)?);
                if local {
                    env.provide_local(&local_service(&id)?, Arc::new(JsObject::new(self.clone(), object)))
                } else {
                    env.provide(&ServiceRef::remote(id), self.service_impl(object, &args.json(5))?)
                }
            }
            "provideMany" => {
                let (id, local) = args.token(3)?;
                let spawner = if local {
                    Spawner::Local(env.provide_many_local(&local_service(&id)?).map_err(js_error)?)
                } else {
                    Spawner::Remote(env.provide_many(&ServiceRef::remote(id)).map_err(js_error)?)
                };
                return Ok(id_value(self.insert(|tables, id| drop(tables.spawners.insert(id, spawner)))));
            }
            "replicatedState" => {
                let state = env.replicated_state(Json::from(&args.json(3))).map_err(js_error)?;
                return Ok(id_value(self.insert(|tables, id| drop(tables.states.insert(id, state)))));
            }
            "own" | "onActivate" => {
                let (weak, function) = (Arc::downgrade(self), args.id(3)?);
                let run = move || async move { upgrade(&weak)?.call_function(function).await };
                if operation == "own" { env.own_async(run) } else { env.on_activate(run) }
            }
            other => return Err(JsError::type_error(format!("Unknown facet environment operation {other}"))),
        };
        result.map(|()| HostValue::Undefined).map_err(js_error)
    }

    async fn observe(self: Arc<Self>, handler: u64, local: bool, handle: u64, context: Context) -> ChordResult<()> {
        let (context_id, listening) = self.context_id(&context);
        let args = vec![id_value(handler), view(local, handle), id_value(context_id)];
        let result = self.api()?.call_method("observe", args).await.map_err(chord_error);
        self.release_context(context_id, listening);
        let mut tables = lock(&self.tables);
        tables.handles.remove(&handle);
        tables.locals.remove(&handle);
        result.map(drop)
    }

    fn spawn(self: &Arc<Self>, args: &Args) -> Result<HostValue, JsError> {
        let (spawner, key, object) = (args.id(1)?, args.text(2)?, args.id(3)?);
        let spawner = match lock(&self.tables).spawners.get(&spawner) {
            Some(Spawner::Remote(spawner)) => Spawner::Remote(spawner.clone()),
            Some(Spawner::Local(spawner)) => Spawner::Local(spawner.clone()),
            None => return Err(JsError::new("Facet service spawner is no longer available")),
        };
        let closer = match spawner {
            Spawner::Remote(spawner) => spawner.spawn(&key, self.service_impl(object, &args.json(4))?),
            Spawner::Local(spawner) => spawner.spawn(&key, Arc::new(JsObject::new(self.clone(), object))),
        };
        Ok(self.closer(closer.map_err(js_error)?))
    }

    fn state(&self, id: u64) -> Result<MutableReplicatedState, JsError> {
        lock(&self.tables).states.get(&id).cloned().ok_or_else(|| JsError::new("Unknown replicated state"))
    }

    fn handle_of(&self, id: u64) -> Result<ServiceHandle, JsError> {
        lock(&self.tables).handles.get(&id).cloned().ok_or_else(|| JsError::new("Service handle is closed"))
    }

    /// The synchronous `chord` host function.
    pub fn dispatch(self: &Arc<Self>, args: Vec<HostValue>) -> Result<HostValue, JsError> {
        let args = Args(args);
        match args.text(0)?.as_str() {
            "env" => self.environment(&args),
            "spawn" => self.spawn(&args),
            "close" | "unsubscribe" => {
                let closer = lock(&self.tables).closers.remove(&args.id(1)?);
                closer.map_or(Ok(()), |closer| closer.close()).map_err(js_error)?;
                Ok(HostValue::Undefined)
            }
            "state.create" => {
                let state = replicated_state(Json::from(&args.json(1)));
                Ok(id_value(self.insert(|tables, id| drop(tables.states.insert(id, state)))))
            }
            "state.value" => Ok(host_json(Some(self.state(args.id(1)?)?.value()))),
            "state.replace" => {
                let value = Json::from(&args.json(2));
                self.state(args.id(1)?)?.replace(&BACKGROUND_CONTEXT, value).map_err(js_error)?;
                Ok(HostValue::Undefined)
            }
            "state.subscribe" => {
                let subscription = self.state(args.id(1)?)?.subscribe(self.listener(args.id(2)?));
                Ok(self.closer(subscription))
            }
            "member.value" => {
                let member = self.handle_of(args.id(1)?)?.state(&args.text(2)?).map_err(js_error)?;
                Ok(host_json(member.value().map_err(js_error)?))
            }
            "member.subscribe" => {
                let member = self.handle_of(args.id(1)?)?.state(&args.text(2)?).map_err(js_error)?;
                Ok(self.closer(member.subscribe(self.listener(args.id(3)?)).map_err(js_error)?))
            }
            "local.get" => {
                let local = lock(&self.tables).locals.get(&args.id(1)?).cloned();
                let local = local.ok_or_else(|| JsError::new("Service handle is closed"))?;
                Ok(id_value(local.get().map_err(js_error)?.id()))
            }
            other => Err(JsError::type_error(format!("Unknown chord bridge operation {other}"))),
        }
    }

    /// The asynchronous `chord.async` host function: remote service method calls.
    pub async fn dispatch_async(self: Arc<Self>, args: Vec<HostValue>) -> Result<HostValue, JsError> {
        let args = Args(args);
        let handle = self.handle_of(args.id(1)?)?;
        let member = args.text(2)?;
        let call_args = args.json(3).as_array().map(|items| items.iter().map(Json::from).collect()).unwrap_or_default();
        let result = handle.call(&member, call_args, &BACKGROUND_CONTEXT).await.map_err(js_error)?;
        Ok(host_json(result))
    }
}
