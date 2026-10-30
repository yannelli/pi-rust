//! `Send` handles for posting work to the JavaScript thread and awaiting results.

use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Arc, mpsc};

use rquickjs::function::Args;
use rquickjs::{Ctx, Function, Object, Value};

use super::convert::{caught, from_host, restore, settled, to_host, to_json, when_settled};
use super::scope::JsScope;
use super::state::{LoopState, Message, Shared, try_state};
use crate::error::JsError;
use crate::host::{HostFunction, HostValue};
use crate::virtual_modules::VirtualModule;

fn shut_down() -> JsError {
    JsError::new("the JavaScript runtime has shut down").with_code("ERR_PI_JS_SHUT_DOWN")
}

enum Reply<R> {
    Async(tokio::sync::oneshot::Sender<Result<R, JsError>>),
    Blocking(mpsc::Sender<Result<R, JsError>>),
}

impl<R> Reply<R> {
    fn send(self, result: Result<R, JsError>) {
        match self {
            Self::Async(sender) => drop(sender.send(result)),
            Self::Blocking(sender) => drop(sender.send(result)),
        }
    }
}

type Produce = Box<dyn for<'js> FnOnce(&Ctx<'js>) -> Result<Value<'js>, JsError> + Send>;

/// A cloneable, `Send + Sync` handle to one JavaScript runtime thread.
#[derive(Clone)]
pub struct JsHandle {
    sender: Sender<Message>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for JsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsHandle").field("alive", &self.is_alive()).finish()
    }
}

impl JsHandle {
    pub(crate) fn new(sender: Sender<Message>, shared: Arc<Shared>) -> Self {
        Self { sender, shared }
    }

    pub(crate) fn from_state(state: &LoopState) -> Self {
        Self::new(state.sender.clone(), state.shared.clone())
    }

    pub(crate) fn belongs_to(&self, shared: &Arc<Shared>) -> bool {
        Arc::ptr_eq(&self.shared, shared)
    }

    pub(crate) fn send(&self, message: Message) -> Result<(), JsError> {
        if !self.is_alive() {
            return Err(shut_down());
        }
        self.sender.send(message).map_err(|_| shut_down())
    }

    /// Whether the caller is the runtime's own JavaScript thread.
    pub fn on_js_thread(&self) -> bool {
        try_state().is_some_and(|state| Arc::ptr_eq(&state.shared, &self.shared))
    }

    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }

    /// Run `f` on the JavaScript thread with direct access to the context.
    pub async fn with<R, F>(&self, f: F) -> Result<R, JsError>
    where
        F: for<'js> FnOnce(&JsScope<'js>) -> Result<R, JsError> + Send + 'static,
        R: Send + 'static,
    {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.send(Message::Task(Box::new(move |ctx| {
            let _ = sender.send(f(&JsScope::new(ctx.clone())));
        })))?;
        receiver.await.map_err(|_| shut_down())?
    }

    /// [`JsHandle::with`] for synchronous callers on other threads.
    pub fn blocking_with<R, F>(&self, f: F) -> Result<R, JsError>
    where
        F: for<'js> FnOnce(&JsScope<'js>) -> Result<R, JsError> + Send + 'static,
        R: Send + 'static,
    {
        self.refuse_on_js_thread()?;
        let (sender, receiver) = mpsc::channel();
        self.send(Message::Task(Box::new(move |ctx| {
            let _ = sender.send(f(&JsScope::new(ctx.clone())));
        })))?;
        receiver.recv().map_err(|_| shut_down())?
    }

    /// Queue `f` on the JavaScript thread without waiting for it.
    pub fn post<F>(&self, f: F) -> Result<(), JsError>
    where
        F: for<'js> FnOnce(&JsScope<'js>) + Send + 'static,
    {
        self.send(Message::Task(Box::new(move |ctx| f(&JsScope::new(ctx.clone())))))
    }

    fn refuse_on_js_thread(&self) -> Result<(), JsError> {
        if self.on_js_thread() {
            return Err(JsError::new("blocking call on the JavaScript thread would deadlock"));
        }
        Ok(())
    }

    fn dispatch<R: Send + 'static>(
        &self,
        reply: Reply<R>,
        produce: Produce,
        convert: impl for<'a> FnOnce(&Ctx<'a>, Value<'a>) -> Result<R, JsError> + Send + 'static,
    ) -> Result<(), JsError> {
        self.send(Message::Task(Box::new(move |ctx| match produce(ctx) {
            Err(error) => reply.send(Err(error)),
            Ok(value) => {
                let mut reply = Some(reply);
                let mut convert = Some(convert);
                let done = settled(move |ctx, result| {
                    if let (Some(reply), Some(convert)) = (reply.take(), convert.take()) {
                        reply.send(result.and_then(|value| convert(ctx, value)));
                    }
                });
                if let Err(error) = when_settled(ctx, value, done) {
                    let _ = caught(ctx, error);
                }
            }
        })))
    }

    pub(crate) async fn request<R: Send + 'static>(
        &self,
        produce: Produce,
        convert: impl for<'a> FnOnce(&Ctx<'a>, Value<'a>) -> Result<R, JsError> + Send + 'static,
    ) -> Result<R, JsError> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.dispatch(Reply::Async(sender), produce, convert)?;
        receiver.await.map_err(|_| shut_down())?
    }

    pub(crate) fn blocking_request<R: Send + 'static>(
        &self,
        produce: Produce,
        convert: impl for<'a> FnOnce(&Ctx<'a>, Value<'a>) -> Result<R, JsError> + Send + 'static,
    ) -> Result<R, JsError> {
        self.blocking_request_within(produce, convert, None)
    }

    /// [`Self::blocking_request`] that gives up after `timeout`; the call still runs later.
    pub(crate) fn blocking_request_within<R: Send + 'static>(
        &self,
        produce: Produce,
        convert: impl for<'a> FnOnce(&Ctx<'a>, Value<'a>) -> Result<R, JsError> + Send + 'static,
        timeout: Option<std::time::Duration>,
    ) -> Result<R, JsError> {
        self.refuse_on_js_thread()?;
        let (sender, receiver) = mpsc::channel();
        self.dispatch(Reply::Blocking(sender), produce, convert)?;
        match timeout {
            None => receiver.recv().map_err(|_| shut_down())?,
            Some(timeout) => match receiver.recv_timeout(timeout) {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => Err(JsError::new("JavaScript call timed out")),
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(shut_down()),
            },
        }
    }

    /// Evaluate a classic script (global scope) and await its value if it is a promise.
    pub async fn eval(&self, source: impl Into<String>) -> Result<HostValue, JsError> {
        let source = source.into();
        self.request(Box::new(move |ctx| JsScope::new(ctx.clone()).eval_script(&source, "<eval>")), to_host).await
    }

    /// Import a module by specifier, resolved from the runtime's cwd; yields its namespace.
    pub async fn import(&self, specifier: impl Into<String>) -> Result<JsRef, JsError> {
        let specifier = specifier.into();
        self.request(Box::new(move |ctx| JsScope::new(ctx.clone()).import_promise(&specifier)), persist_value).await
    }

    /// Import like jiti's `import(path, { default: true })`: the default export when there is
    /// one (`module.exports` for CommonJS), else the namespace.
    pub async fn import_default(&self, specifier: impl Into<String>) -> Result<JsRef, JsError> {
        let specifier = specifier.into();
        let produce: Produce = Box::new(move |ctx| JsScope::new(ctx.clone()).import_promise(&specifier));
        self.request(produce, |ctx, namespace| persist_value(ctx, default_export(ctx, namespace)?)).await
    }

    /// Evaluate ES module source under `name` (a path; `.ts` names are transpiled).
    pub async fn eval_module(&self, name: impl Into<String>, source: impl Into<String>) -> Result<JsRef, JsError> {
        let (name, source) = (name.into(), source.into());
        let produce: Produce = Box::new(move |ctx| JsScope::new(ctx.clone()).module_promise(&name, source));
        self.request(produce, persist_value).await
    }

    pub fn register_virtual_module(&self, name: impl Into<String>, module: VirtualModule) {
        self.shared.virtual_modules.write().unwrap_or_else(|poison| poison.into_inner()).insert(name, module);
    }

    /// Make module files load from memory: each `(path, source)` pair acts as a file at that
    /// absolute path, so relative imports between registered files resolve.
    pub fn register_source_files(&self, files: impl IntoIterator<Item = (std::path::PathBuf, String)>) {
        let mut registered = self.shared.source_files.write().unwrap_or_else(|poison| poison.into_inner());
        for (path, source) in files {
            let key = crate::resolve::normalize(&path).display().to_string();
            registered.insert(key, source.into());
        }
    }

    pub fn register_host_function(&self, name: impl Into<String>, function: HostFunction) {
        self.shared.host_functions.write().unwrap_or_else(|poison| poison.into_inner()).insert(name, function);
    }

    /// Resolves once no timers, immediates, async operations, or jobs remain.
    pub async fn idle(&self) -> Result<(), JsError> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.send(Message::Idle(sender))?;
        receiver.await.map_err(|_| shut_down())
    }

    /// Abort the JavaScript that is running now with an uncatchable `InternalError`.
    pub fn interrupt(&self) {
        self.shared.interrupt.store(true, Ordering::SeqCst);
        let _ = self.sender.send(Message::Task(Box::new(|_| {})));
    }
}

fn persist_value<'a>(ctx: &Ctx<'a>, value: Value<'a>) -> Result<JsRef, JsError> {
    Ok(super::convert::persist(ctx, value))
}

fn persist_present<'a>(ctx: &Ctx<'a>, value: Value<'a>) -> Result<Option<JsRef>, JsError> {
    Ok((!value.is_undefined() && !value.is_null()).then(|| super::convert::persist(ctx, value)))
}

pub(crate) fn default_export<'a>(ctx: &Ctx<'a>, namespace: Value<'a>) -> Result<Value<'a>, JsError> {
    let Some(object) = namespace.as_object() else { return Ok(namespace) };
    let value: Value = object.get("default").map_err(|error| caught(ctx, error))?;
    Ok(if value.is_undefined() || value.is_null() { namespace } else { value })
}

/// A JavaScript value kept alive for Rust. Dropping the last clone releases it.
#[derive(Clone)]
pub struct JsRef {
    inner: Arc<RefInner>,
}

struct RefInner {
    id: u64,
    handle: JsHandle,
}

impl Drop for RefInner {
    fn drop(&mut self) {
        let _ = self.handle.sender.send(Message::Release(self.id));
    }
}

impl std::fmt::Debug for JsRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("JsRef").field(&self.inner.id).finish()
    }
}

impl JsRef {
    pub(crate) fn new(id: u64, handle: JsHandle) -> Self {
        Self { inner: Arc::new(RefInner { id, handle }) }
    }

    pub fn id(&self) -> u64 {
        self.inner.id
    }

    pub fn handle(&self) -> &JsHandle {
        &self.inner.handle
    }

    fn invoke(&self, method: Option<String>, args: Vec<HostValue>) -> Produce {
        let target = self.clone();
        Box::new(move |ctx| {
            let value = restore(ctx, &target)?;
            let (function, this) = match &method {
                Some(name) => {
                    let object = value.as_object().ok_or_else(|| JsError::type_error("value is not an object"))?;
                    let function: Value = object.get(name.as_str()).map_err(|error| caught(ctx, error))?;
                    (function, value.clone())
                }
                None => (value, Value::new_undefined(ctx.clone())),
            };
            let label = method.as_deref().unwrap_or("value");
            let function: Function =
                function.into_function().ok_or_else(|| JsError::type_error(format!("{label} is not a function")))?;
            let mut call_args = Args::new(ctx.clone(), args.len());
            call_args.this(this).map_err(|error| caught(ctx, error))?;
            for arg in args {
                call_args.push_arg(from_host(ctx, arg)?).map_err(|error| caught(ctx, error))?;
            }
            function.call_arg(call_args).map_err(|error| caught(ctx, error))
        })
    }

    /// Call the value as a function; promise results are awaited.
    pub async fn call(&self, args: Vec<HostValue>) -> Result<HostValue, JsError> {
        self.handle().request(self.invoke(None, args), to_host).await
    }

    /// Like [`JsRef::call`] but keeps the result as a reference (objects with methods).
    pub async fn call_ref(&self, args: Vec<HostValue>) -> Result<JsRef, JsError> {
        self.handle().request(self.invoke(None, args), persist_value).await
    }

    pub async fn call_method(&self, name: &str, args: Vec<HostValue>) -> Result<HostValue, JsError> {
        self.handle().request(self.invoke(Some(name.into()), args), to_host).await
    }

    pub async fn call_method_ref(&self, name: &str, args: Vec<HostValue>) -> Result<JsRef, JsError> {
        self.handle().request(self.invoke(Some(name.into()), args), persist_value).await
    }

    pub async fn get(&self, key: &str) -> Result<HostValue, JsError> {
        self.handle().request(self.property(key), to_host).await
    }

    /// [`JsRef::get`] for synchronous callers on other threads.
    pub fn blocking_get(&self, key: &str) -> Result<HostValue, JsError> {
        self.handle().blocking_request(self.property(key), to_host)
    }

    /// Set a property of the object.
    pub async fn set(&self, key: &str, value: HostValue) -> Result<(), JsError> {
        self.handle().request(self.assign(key, value), |_, _| Ok(())).await
    }

    pub fn blocking_set(&self, key: &str, value: HostValue) -> Result<(), JsError> {
        self.handle().blocking_request(self.assign(key, value), |_, _| Ok(()))
    }

    pub async fn get_ref(&self, key: &str) -> Result<JsRef, JsError> {
        self.handle().request(self.property(key), persist_value).await
    }

    /// The value through `JSON.stringify`, without awaiting promises.
    pub async fn to_json(&self) -> Result<serde_json::Value, JsError> {
        let target = self.clone();
        self.handle().with(move |scope| to_json(scope.ctx(), restore(scope.ctx(), &target)?)).await
    }

    /// [`JsRef::call`] for synchronous callers on other threads (for example render calls).
    pub fn blocking_call(&self, args: Vec<HostValue>) -> Result<HostValue, JsError> {
        self.handle().blocking_request(self.invoke(None, args), to_host)
    }

    pub fn blocking_call_method(&self, name: &str, args: Vec<HostValue>) -> Result<HostValue, JsError> {
        self.handle().blocking_request(self.invoke(Some(name.into()), args), to_host)
    }

    pub fn blocking_call_method_ref(&self, name: &str, args: Vec<HostValue>) -> Result<JsRef, JsError> {
        self.handle().blocking_request(self.invoke(Some(name.into()), args), persist_value)
    }

    /// [`JsRef::blocking_call_method`] that fails after `timeout` instead of waiting longer.
    pub fn blocking_call_method_within(
        &self,
        name: &str,
        args: Vec<HostValue>,
        timeout: std::time::Duration,
    ) -> Result<HostValue, JsError> {
        self.handle().blocking_request_within(self.invoke(Some(name.into()), args), to_host, Some(timeout))
    }

    /// Like [`JsRef::call_method_ref`], with `None` for an `undefined` or `null` result.
    pub async fn call_method_opt_ref(&self, name: &str, args: Vec<HostValue>) -> Result<Option<JsRef>, JsError> {
        self.handle().request(self.invoke(Some(name.into()), args), persist_present).await
    }

    pub fn blocking_call_method_opt_ref(&self, name: &str, args: Vec<HostValue>) -> Result<Option<JsRef>, JsError> {
        self.handle().blocking_request(self.invoke(Some(name.into()), args), persist_present)
    }

    fn assign(&self, key: &str, value: HostValue) -> Produce {
        let (target, key) = (self.clone(), key.to_string());
        Box::new(move |ctx| {
            let object: Object =
                restore(ctx, &target)?.into_object().ok_or_else(|| JsError::type_error("value is not an object"))?;
            object.set(key.as_str(), from_host(ctx, value)?).map_err(|error| caught(ctx, error))?;
            Ok(Value::new_undefined(ctx.clone()))
        })
    }

    fn property(&self, key: &str) -> Produce {
        let (target, key) = (self.clone(), key.to_string());
        Box::new(move |ctx| {
            let value = restore(ctx, &target)?;
            let object: Object = value.into_object().ok_or_else(|| JsError::type_error("value is not an object"))?;
            object.get(key.as_str()).map_err(|error| caught(ctx, error))
        })
    }
}
