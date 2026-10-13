//! State owned by the JavaScript thread, reached through a thread-local.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};

use rquickjs::{Ctx, Function, Object, Persistent, Value};

use super::timers::Timers;
use super::{OutputSink, RuntimeConfig};
use crate::error::JsError;
use crate::host::HostFunctions;
use crate::resolve::ModuleResolver;
use crate::transpile::TranspileCache;
use crate::virtual_modules::VirtualModules;

/// Work for the JavaScript thread, run inside the context.
pub(crate) type Task = Box<dyn for<'js> FnOnce(&Ctx<'js>) + Send>;

pub(crate) enum Message {
    Task(Task),
    /// A `JsRef` was dropped on some thread.
    Release(u64),
    /// Reply once nothing keeps the event loop busy.
    Idle(tokio::sync::oneshot::Sender<()>),
    Shutdown,
}

/// State reachable from any thread through a `JsHandle`.
pub(crate) struct Shared {
    pub tokio: tokio::runtime::Handle,
    pub virtual_modules: RwLock<VirtualModules>,
    pub host_functions: RwLock<HostFunctions>,
    pub transpiler: TranspileCache,
    pub resolver: ModuleResolver,
    pub interrupt: Arc<AtomicBool>,
    pub alive: AtomicBool,
    pub config: RuntimeConfig,
    pub output: Arc<dyn OutputSink>,
    /// Files opened through `fs.open`, by descriptor.
    pub files: crate::node::fs::FileTable,
    pub http: crate::node::fetch::Clients,
    /// Module sources registered by path; they load as if the files existed.
    pub source_files: RwLock<HashMap<String, Arc<str>>>,
}

pub(crate) struct PendingPromise {
    pub resolve: Persistent<Function<'static>>,
    pub reject: Persistent<Function<'static>>,
}

/// Everything holding JavaScript values lives here, so shutdown can release it all before the
/// QuickJS runtime is freed.
pub(crate) struct LoopState {
    pub shared: Arc<Shared>,
    pub sender: Sender<Message>,
    next_id: Cell<u64>,
    pub refs: RefCell<HashMap<u64, Persistent<Value<'static>>>>,
    pub promises: RefCell<HashMap<u64, PendingPromise>>,
    pub callbacks: RefCell<HashMap<u64, Persistent<Function<'static>>>>,
    /// Rust-side resources (child processes, hashes, file handles) by id.
    pub resources: RefCell<HashMap<u64, Box<dyn Any>>>,
    /// Async operations and resources that keep the loop busy.
    pub busy: Cell<usize>,
    pub timers: RefCell<Timers>,
    pub immediates: RefCell<VecDeque<(u64, Persistent<Function<'static>>)>>,
    pub rejections: RefCell<Vec<(Persistent<Value<'static>>, Persistent<Value<'static>>)>>,
    /// The bootstrap's internal API object (`__pi_internal`).
    pub internal: RefCell<Option<Persistent<Object<'static>>>>,
    /// Source text for modules evaluated from strings, by module name.
    pub inline_sources: RefCell<HashMap<String, String>>,
    pub idle_waiters: RefCell<Vec<tokio::sync::oneshot::Sender<()>>>,
    pub shutting_down: Cell<bool>,
    /// `process.nextTick` callbacks are queued; they run before the next microtasks.
    pub ticks_pending: Cell<bool>,
    pub cwd: RefCell<PathBuf>,
}

thread_local! {
    static STATE: RefCell<Option<Rc<LoopState>>> = const { RefCell::new(None) };
}

impl LoopState {
    pub fn install(shared: Arc<Shared>, sender: Sender<Message>) -> Rc<Self> {
        let cwd = shared.config.cwd.clone();
        let state = Rc::new(Self {
            shared,
            sender,
            next_id: Cell::new(1),
            refs: RefCell::default(),
            promises: RefCell::default(),
            callbacks: RefCell::default(),
            resources: RefCell::default(),
            busy: Cell::new(0),
            timers: RefCell::default(),
            immediates: RefCell::default(),
            rejections: RefCell::default(),
            internal: RefCell::default(),
            inline_sources: RefCell::default(),
            idle_waiters: RefCell::default(),
            shutting_down: Cell::new(false),
            ticks_pending: Cell::new(false),
            cwd: RefCell::new(cwd),
        });
        STATE.with(|slot| *slot.borrow_mut() = Some(state.clone()));
        state
    }

    /// Drop every persistent value; required before the QuickJS runtime is freed.
    pub fn release_all(&self) {
        self.refs.borrow_mut().clear();
        self.promises.borrow_mut().clear();
        self.callbacks.borrow_mut().clear();
        self.resources.borrow_mut().clear();
        self.timers.borrow_mut().clear();
        self.immediates.borrow_mut().clear();
        self.rejections.borrow_mut().clear();
        self.internal.borrow_mut().take();
        STATE.with(|slot| slot.borrow_mut().take());
    }

    pub fn next_id(&self) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        id
    }

    pub fn add_busy(&self) {
        self.busy.set(self.busy.get() + 1);
    }

    pub fn remove_busy(&self) {
        self.busy.set(self.busy.get().saturating_sub(1));
    }

    pub fn internal<'js>(&self, ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
        let internal = self.internal.borrow().clone();
        match internal {
            Some(object) => object.restore(ctx),
            None => Err(rquickjs::Exception::throw_internal(ctx, "pi-js bootstrap has not run")),
        }
    }

    pub fn insert_resource(&self, resource: Box<dyn Any>) -> u64 {
        let id = self.next_id();
        self.resources.borrow_mut().insert(id, resource);
        id
    }

    /// Run `f` on a resource of type `T`, failing when it is gone or of another type.
    pub fn with_resource<T: 'static, R>(&self, id: u64, f: impl FnOnce(&mut T) -> R) -> Result<R, JsError> {
        let mut resources = self.resources.borrow_mut();
        let resource = resources
            .get_mut(&id)
            .and_then(|resource| resource.downcast_mut::<T>())
            .ok_or_else(|| JsError::new(format!("resource {id} is closed")).with_code("ERR_INVALID_STATE"))?;
        Ok(f(resource))
    }
}

/// The state of the current JavaScript thread. Only called from code running on that thread.
pub(crate) fn state() -> Rc<LoopState> {
    STATE.with(|slot| slot.borrow().clone()).expect("pi-js state used outside its JavaScript thread")
}

/// The state when called on a JavaScript thread, for code that may run elsewhere.
pub(crate) fn try_state() -> Option<Rc<LoopState>> {
    STATE.with(|slot| slot.borrow().clone())
}
