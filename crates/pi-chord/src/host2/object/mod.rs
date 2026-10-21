//! Port of upstream `object.ts`: services, what a factory declares, call contexts, references and the objects
//! that serve them. Upstream's `Self` (what a factory declares through) is [`Declare`] here, as `Self` is a Rust
//! keyword. Objects reach their host through [`HostCore`], which the host implements.

mod activate;
mod calls;
mod ctx;
mod declare;
mod handle;
mod lifetime;
mod member;
mod reference;
mod resource;
mod runtime;
mod service;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use indexmap::IndexMap;
use tokio::runtime::Handle;
use tokio::sync::Notify;

pub(crate) use calls::clean;
pub use ctx::{Ctx, RootOptions};
pub(crate) use ctx::{CtxFields, await_ready, run_root};
pub use declare::Decls;
pub(crate) use declare::Run;
pub use declare::{Declare, InitFn, Initial, Listen, Listenable, Migrate, StateOptions, TaskFn};
pub(crate) use handle::notify_status;
pub use handle::{Ref, RefCore, RefOwner};
pub use reference::RefImpl;
pub use resource::{AnyValue, ResourceHandle, ResourceImpl};
pub(crate) use runtime::{StagedListeners, Subscription};
pub use service::{Adapter, Factory, Implementation, Lifetime, Member, MethodFn, ServeOptions, Service};
pub(crate) use service::{make_service, member_schemas};

use super::cell::WriteLock;
use super::contract::Contract;
use super::exec::ErrorHandler;
use super::types::GetOptions;
use crate::context::AbortController;
use crate::error::{ChordError, ChordResult};
use crate::json::Json;
use crate::sync::{BoxFuture, SharedTask, lock};

/// A root's body as the host runs it: the value travels through the closure (see [`run_root`]).
pub type RootFn = Box<dyn FnOnce(Ctx) -> BoxFuture<ChordResult<()>> + Send>;

/// What objects, references and contexts need from their host (upstream's `HostCore`).
pub trait HostCore: Send + Sync + 'static {
    /// Process-unique: contexts and factory frames compare hosts by it.
    fn id(&self) -> u64;
    fn name(&self) -> &str;
    fn dev(&self) -> bool;
    /// The identity of the host's own code.
    fn principal(&self) -> &Json;
    fn handle(&self) -> Option<&Handle>;
    /// Set when a commit failed midway: the host admits no more work.
    fn failed(&self) -> Option<ChordError>;
    fn report(&self, error: ChordError);
    fn current_code_version(&self) -> i64;
    fn has_code_version(&self, code_version: i64) -> bool;
    /// Every code version the host keeps (and a running reload's pending one) with its service for `type_name`.
    fn retained_services(&self, type_name: &str) -> Vec<(i64, Option<Arc<Service>>)>;
    fn pending_code_version(&self) -> Option<i64>;
    fn service(&self, type_name: &str) -> Option<Arc<Service>>;
    fn acquire(
        &self,
        contract: &Contract,
        input: &Json,
        options: &GetOptions,
        owner: RefOwner,
        from: Option<&Arc<ObjectImpl>>,
        candidate: bool,
    ) -> ChordResult<Ref>;
    fn address_of(&self, contract: &Contract, input: &Json, principal: &Json, candidate: bool) -> ChordResult<String>;
    /// Starts a root: its id is taken now, `run` runs when the returned future is first polled.
    fn root_erased(&self, run: RootFn, options: RootOptions) -> BoxFuture<ChordResult<()>>;
    fn touch(&self, root_id: &str, object: &Arc<ObjectImpl>);
    fn forget(&self, object: &ObjectImpl);
    /// The object finished its disposal.
    fn disposed(&self, object: &ObjectImpl);
    /// How long a reload waits for aborted tasks before starting their replacements.
    fn task_stop_deadline(&self) -> Duration;
    /// How long an activation waits for its references to be ready and hydrated; `None` waits forever.
    fn ready_timeout(&self) -> Option<Duration>;
    fn check_registrations(&self, object: &Arc<ObjectImpl>, decls: &Decls) -> ChordResult<()>;
    /// Rebuilds the current registry tables from live objects (plus `including`, finishing activation).
    fn registries_changed(&self, including: Option<&Arc<ObjectImpl>>);
}

/// Where an object is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectStatus {
    Activating,
    Live,
    Failed,
    Disposing,
    Disposed,
}

impl ObjectStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Activating => "activating",
            Self::Live => "live",
            Self::Failed => "failed",
            Self::Disposing => "disposing",
            Self::Disposed => "disposed",
        }
    }
}

/// An implementation installed for one code version (the key of `implementations`).
#[derive(Clone)]
pub(crate) struct Installed {
    pub service: Arc<Service>,
    pub implementation: Arc<Implementation>,
    pub decls: Arc<Decls>,
}

/// A reload gate: calls of roots not grandfathered are refused, listener events wait.
pub(crate) struct Gate {
    pub grandfathered: HashSet<String>,
    pub queue: Vec<(String, Json)>,
    pub drained: Option<Box<dyn FnOnce() + Send>>,
}

pub(crate) struct RunningTask {
    pub controller: AbortController,
    pub done: SharedTask,
}

pub(crate) struct ObjectState {
    pub status: ObjectStatus,
    pub service: Option<Arc<Service>>,
    /// Calls carrying an older code version are refused `upgrade_conflict`.
    pub min_code_version: i64,
    pub decls: Arc<Decls>,
    /// Declarations of factory runs not installed yet (activation or reload candidate).
    pub pending: Vec<Arc<Decls>>,
    pub implementations: IndexMap<i64, Installed>,
    pub refs: IndexMap<usize, Arc<RefImpl>>,
    pub leases: HashMap<String, i64>,
    pub gate: Option<Gate>,
    /// Served only by a running reload's pending code version: runtime starts at its commit.
    pub deferred_runtime: bool,
    pub subscriptions: IndexMap<String, Subscription>,
    pub invocations: usize,
    pub tasks: IndexMap<String, RunningTask>,
    /// Aborted task runs that have not finished yet.
    pub stopped: IndexMap<u64, SharedTask>,
    pub stopped_seq: u64,
    /// Bumped by every start and stop of the tasks: a delayed start that is no longer the latest is dropped.
    pub task_runs: u64,
    /// The running linger timer: its sequence number and (on a runtime) its abort handle.
    pub linger: Option<(u64, Option<tokio::task::AbortHandle>)>,
    pub linger_seq: u64,
    pub disposal: Option<SharedTask>,
}

/// What a host knows about an object before it activates.
pub(crate) struct ObjectIdentity {
    pub address: String,
    pub type_name: String,
    pub input: Json,
    pub principal: Json,
    /// The one object of a `lifetime: "host"` type: only such objects may register.
    pub host_lifetime: bool,
}

/// One object: an address on a host, its declarations, implementations and references.
pub struct ObjectImpl {
    host: Arc<dyn HostCore>,
    identity: ObjectIdentity,
    pub(crate) call_abort: AbortController,
    pub(crate) activation_abort: AbortController,
    pub(crate) write_lock: Arc<WriteLock>,
    ready: Mutex<SharedTask>,
    pub(crate) open_order: Mutex<Vec<Arc<ResourceImpl>>>,
    state: Mutex<ObjectState>,
    /// Notified when the last running invocation settles.
    idle: Notify,
    me: Weak<ObjectImpl>,
}

impl ObjectImpl {
    /// A new object; its activation starts once [`ObjectImpl::launch`] runs, after `after` settles.
    pub(crate) fn new(host: Arc<dyn HostCore>, identity: ObjectIdentity, after: Option<SharedTask>) -> Arc<Self> {
        Arc::new_cyclic(|me: &Weak<Self>| {
            let ready = activate::activation(me.clone(), after);
            Self {
                host,
                identity,
                call_abort: AbortController::new(),
                activation_abort: AbortController::new(),
                write_lock: Arc::default(),
                ready: Mutex::new(ready),
                open_order: Mutex::default(),
                state: Mutex::new(ObjectState {
                    status: ObjectStatus::Activating,
                    service: None,
                    min_code_version: 0,
                    decls: Arc::default(),
                    pending: Vec::new(),
                    implementations: IndexMap::new(),
                    refs: IndexMap::new(),
                    leases: HashMap::new(),
                    gate: None,
                    deferred_runtime: false,
                    subscriptions: IndexMap::new(),
                    invocations: 0,
                    tasks: IndexMap::new(),
                    stopped: IndexMap::new(),
                    stopped_seq: 0,
                    task_runs: 0,
                    linger: None,
                    linger_seq: 0,
                    disposal: None,
                }),
                idle: Notify::new(),
                me: me.clone(),
            }
        })
    }

    pub fn address(&self) -> &str {
        &self.identity.address
    }

    pub fn type_name(&self) -> &str {
        &self.identity.type_name
    }

    pub fn input(&self) -> &Json {
        &self.identity.input
    }

    pub fn principal(&self) -> &Json {
        &self.identity.principal
    }

    pub fn host_lifetime(&self) -> bool {
        self.identity.host_lifetime
    }

    pub fn status(&self) -> ObjectStatus {
        self.state().status
    }

    /// How many references hold the object now.
    pub fn ref_count(&self) -> usize {
        self.state().refs.len()
    }

    pub(crate) fn host(&self) -> &Arc<dyn HostCore> {
        &self.host
    }

    pub(crate) fn me(&self) -> Option<Arc<Self>> {
        self.me.upgrade()
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, ObjectState> {
        lock(&self.state)
    }

    /// Settles once the activation does.
    pub(crate) fn ready(&self) -> SharedTask {
        lock(&self.ready).clone()
    }

    pub(crate) fn report(&self) -> ErrorHandler {
        let host = self.host.clone();
        Arc::new(move |error| host.report(error))
    }

    pub(crate) fn decls(&self) -> Arc<Decls> {
        self.state().decls.clone()
    }

    pub(crate) fn deferred_runtime(&self) -> bool {
        self.state().deferred_runtime
    }

    pub(crate) fn installed(&self, code_version: i64) -> Option<Installed> {
        self.state().implementations.get(&code_version).cloned()
    }

    /// The implementation of the current code version, or the latest installed one.
    pub(crate) fn current_installed(&self) -> Option<Installed> {
        let current = self.host.current_code_version();
        let state = self.state();
        state.implementations.get(&current).or_else(|| state.implementations.values().last()).cloned()
    }

    /// The declarations of every installed or pending run, each once.
    pub(crate) fn all_decls(&self) -> Vec<Arc<Decls>> {
        let state = self.state();
        let mut result: Vec<Arc<Decls>> = Vec::new();
        let installed = state.implementations.values().map(|installed| &installed.decls);
        for decls in std::iter::once(&state.decls).chain(state.pending.iter()).chain(installed) {
            if !result.iter().any(|existing| Arc::ptr_eq(existing, decls)) {
                result.push(decls.clone());
            }
        }
        result
    }

    /// The objects this one holds through object-owned references on this host.
    pub(crate) fn held_objects(&self) -> Vec<Arc<ObjectImpl>> {
        let mut result: Vec<Arc<ObjectImpl>> = Vec::new();
        for decls in self.all_decls() {
            let refs: Vec<Ref> = decls.data().refs.values().cloned().collect();
            for reference in refs {
                if let Some(object) = reference.object()
                    && !result.iter().any(|existing| Arc::ptr_eq(existing, &object))
                {
                    result.push(object);
                }
            }
        }
        result
    }

    /// Every state source exposed as a remote state member gets that member's schema (dev-mode write checks).
    pub(crate) fn refresh_schemas(&self) {
        let installed: Vec<Installed> = self.state().implementations.values().cloned().collect();
        let mut schemas: HashMap<usize, Vec<Json>> = HashMap::new();
        for installed in &installed {
            for (source, list) in member_schemas(&installed.service, &installed.implementation) {
                schemas.entry(source).or_default().extend(list);
            }
        }
        for decls in self.all_decls() {
            let data = decls.data();
            let cores = data.cells.values().map(|cell| cell.core()).chain(data.derives.values().map(|d| d.core()));
            for core in cores {
                core.set_schemas(schemas.get(&core.id()).cloned().unwrap_or_default());
            }
        }
    }
}

impl std::fmt::Debug for ObjectImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectImpl").field("address", &self.identity.address).finish()
    }
}

/// True if `from` holds `to`, directly or through other objects of this host.
pub(crate) fn reaches(from: &Arc<ObjectImpl>, to: &Arc<ObjectImpl>) -> bool {
    let mut seen: Vec<Arc<ObjectImpl>> = Vec::new();
    let mut stack = vec![from.clone()];
    while let Some(object) = stack.pop() {
        if Arc::ptr_eq(&object, to) {
            return true;
        }
        if seen.iter().any(|existing| Arc::ptr_eq(existing, &object)) {
            continue;
        }
        stack.extend(object.held_objects());
        seen.push(object);
    }
    false
}
