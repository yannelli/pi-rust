//! Port of upstream `host.ts`: serving types, facets, objects by address, roots, registries, principals,
//! links and remote references, and closing. Reloads come in phase 4; their seams are the code version fields
//! (`pending`, `gating`, `reloading`) and the retirement fields.

mod address;
mod close;
mod facet;
mod links;
mod registry;
mod roots;

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use indexmap::IndexMap;
use tokio::runtime::Handle;

pub use close::PrincipalMatch;
pub use facet::{Facet, FacetApi, FacetSetup, LoadOptions, define_facet};
pub use links::LingerOptions;
pub use registry::HostRegistry;

use super::connection::{ConnId, ConnectOptions, NetHandle, Route};
use super::contract::Contract;
use super::exec::ErrorHandler;
use super::link::Link;
use super::object::{
    Ctx, Declare, Decls, HostCore, Implementation, ObjectImpl, Ref, RefOwner, RootFn, RootOptions, ServeOptions,
    Service,
};
use super::remote::RemoteBinding;
use super::schema::canonical_json;
use super::types::GetOptions;
use super::wire::PeerKind;
use crate::context::AbortController;
use crate::error::{ChordError, ChordResult, report_unhandled};
use crate::json::Json;
use crate::sync::{BoxFuture, lock};

/// Maps an identity to its object key (should include connection provenance and grant revision).
pub type PrincipalKey = Arc<dyn Fn(&Json) -> String + Send + Sync>;

/// `create_host` options.
#[derive(Clone)]
pub struct HostOptions {
    /// Default `host`.
    pub name: Option<String>,
    /// The identity of the host's own code (upstream `self`): roots it starts, code inside its objects and its
    /// `lifetime: "host"` objects. Compared by `principal_key`.
    pub principal: Json,
    /// Default: canonical JSON.
    pub principal_key: Option<PrincipalKey>,
    /// Validates every cell write and exposed state value against its schemas.
    pub dev: bool,
    /// Errors from background work (disposal, listeners, tasks). Default: the unhandled error hook.
    pub on_error: Option<ErrorHandler>,
    /// How long a reload waits for an object's aborted tasks before starting their replacements (default 5 s).
    pub task_stop_deadline: Duration,
    /// How long an activation, or an awaited `get`, waits for its references (default 60 s; `None`: forever).
    pub ready_timeout: Option<Duration>,
    /// `Client` (browser): never a reload participant, never carries roots or code versions. Default `Host`.
    pub kind: PeerKind,
    /// Remote bindings kept after their last reference is released, by type.
    pub linger: LingerOptions,
    /// The link to this host's parent (server). Can also be set later with `connect`.
    pub upstream: Option<(Arc<dyn Link>, ConnectOptions)>,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            name: None,
            principal: Json::Null,
            principal_key: None,
            dev: false,
            on_error: None,
            task_stop_deadline: Duration::from_secs(5),
            ready_timeout: Some(Duration::from_secs(60)),
            kind: PeerKind::Host,
            linger: LingerOptions::default(),
            upstream: None,
        }
    }
}

type ErasedValue = Arc<dyn Any + Send + Sync>;
/// A registry table with its values type-erased; views downcast them.
pub(crate) type ErasedTable = Arc<IndexMap<String, ErasedValue>>;
type ErasedListener = Arc<dyn Fn(&ErasedTable, i64) + Send + Sync>;

/// One code version: its services, registry tables, running roots, and the abort of a forced retirement.
pub(crate) struct CodeVersion {
    pub id: i64,
    pub services: IndexMap<String, Arc<Service>>,
    pub tables: Arc<HashMap<String, ErasedTable>>,
    /// Running roots with the number of their calls running here.
    pub roots: HashMap<String, usize>,
    pub abort: AbortController,
}

impl CodeVersion {
    fn new(id: i64) -> Self {
        let tables = Arc::default();
        Self { id, services: IndexMap::new(), tables, roots: HashMap::new(), abort: AbortController::new() }
    }
}

pub(crate) struct HostState {
    pub objects: IndexMap<String, Arc<ObjectImpl>>,
    /// Objects being disposed, by address: a new object there waits for that cleanup.
    pub dying: HashMap<String, Arc<ObjectImpl>>,
    pub code_versions: IndexMap<i64, CodeVersion>,
    pub current: i64,
    /// Reload seams (phase 4): the code version a running reload commits, and the types behind its gate.
    pub pending: Option<CodeVersion>,
    pub gating: Option<HashSet<String>>,
    pub reloading: bool,
    pub facets: IndexMap<String, Facet>,
    /// Aborted when the installed run of a facet is replaced, removed, or the host closes.
    pub facet_runs: HashMap<String, AbortController>,
    /// The host's own references to its `lifetime: "host"` objects, by type name.
    pub host_objects: IndexMap<String, Ref>,
    /// Roots started on this host that have not ended.
    pub local: HashSet<String>,
    pub touches: HashMap<String, Vec<Weak<ObjectImpl>>>,
    /// First registration position of every key, by registry: tables keep it.
    pub order: HashMap<String, HashMap<String, usize>>,
    pub subscribers: HashMap<String, IndexMap<u64, ErasedListener>>,
    pub next_subscriber: u64,
    /// Shared bindings of this host's remote references, by key (re-held after an upstream reconnect).
    pub bindings: IndexMap<String, Arc<RemoteBinding>>,
    /// Bindings without references kept for a while, least recently released first.
    pub lingering: IndexMap<String, Arc<RemoteBinding>>,
    pub routes: IndexMap<String, Route>,
    /// Roots started elsewhere with the links they came over: their leases end once every link ended them.
    pub foreign: HashMap<String, HashSet<ConnId>>,
    /// The links calls of a root went out on: they hear when it ends.
    pub root_links: HashMap<String, HashSet<ConnId>>,
    /// The newest code version the parent allowed to retire (a reload seam).
    pub retire_allowed: i64,
}

static HOST_IDS: AtomicU64 = AtomicU64::new(1);

pub(crate) struct HostImpl {
    id: u64,
    name: String,
    dev: bool,
    principal: Json,
    principal_key: PrincipalKey,
    /// The key of the host's own identity: an equal identity is the host's own, whatever object carries it.
    self_key: String,
    on_error: ErrorHandler,
    task_stop_deadline: Duration,
    ready_timeout: Option<Duration>,
    handle: Option<Handle>,
    kind: PeerKind,
    linger: LingerOptions,
    linger_seq: AtomicU64,
    /// The task that owns the links, started with the first one.
    net: OnceLock<NetHandle>,
    /// Keeps root ids unique across restarts of a host with the same name.
    incarnation: String,
    root_seq: AtomicU64,
    failed: Mutex<Option<ChordError>>,
    state: Mutex<HostState>,
    me: Weak<HostImpl>,
}

/// Six base-36 characters from the system's random source.
fn incarnation() -> String {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 8];
    let _ = ring::rand::SystemRandom::new().fill(&mut bytes);
    let mut value = u64::from_le_bytes(bytes);
    let digits: Vec<char> = (0..6)
        .map(|_| {
            let digit = (value % 36) as u32;
            value /= 36;
            char::from_digit(digit, 36).unwrap_or('0')
        })
        .collect();
    digits.into_iter().collect()
}

/// A host: the objects of the types it serves, and the roots that call them. Cheap to clone.
#[derive(Clone)]
pub struct Host(Arc<HostImpl>);

/// Creates a host. Captures the current Tokio runtime, if any, to run background work on.
pub fn create_host(options: HostOptions) -> Host {
    let principal_key = options.principal_key.unwrap_or_else(|| Arc::new(canonical_json));
    let on_error = options.on_error.unwrap_or_else(|| Arc::new(|error| report_unhandled(&error)));
    let mut code_versions = IndexMap::new();
    code_versions.insert(1, CodeVersion::new(1));
    let state = HostState {
        objects: IndexMap::new(),
        dying: HashMap::new(),
        code_versions,
        current: 1,
        pending: None,
        gating: None,
        reloading: false,
        facets: IndexMap::new(),
        facet_runs: HashMap::new(),
        host_objects: IndexMap::new(),
        local: HashSet::new(),
        touches: HashMap::new(),
        order: HashMap::new(),
        subscribers: HashMap::new(),
        next_subscriber: 0,
        bindings: IndexMap::new(),
        lingering: IndexMap::new(),
        routes: IndexMap::new(),
        foreign: HashMap::new(),
        root_links: HashMap::new(),
        retire_allowed: 0,
    };
    let host = Host(Arc::new_cyclic(|me| HostImpl {
        id: HOST_IDS.fetch_add(1, Ordering::Relaxed),
        name: options.name.unwrap_or_else(|| "host".into()),
        dev: options.dev,
        self_key: principal_key(&options.principal),
        principal: options.principal,
        principal_key,
        on_error,
        task_stop_deadline: options.task_stop_deadline,
        ready_timeout: options.ready_timeout,
        handle: Handle::try_current().ok(),
        kind: options.kind,
        linger: options.linger,
        linger_seq: AtomicU64::new(0),
        net: OnceLock::new(),
        incarnation: incarnation(),
        root_seq: AtomicU64::new(0),
        failed: Mutex::default(),
        state: Mutex::new(state),
        me: me.clone(),
    }));
    if let Some((link, options)) = options.upstream {
        host.connect(link, options);
    }
    host
}

impl HostImpl {
    pub(crate) fn state(&self) -> MutexGuard<'_, HostState> {
        lock(&self.state)
    }

    pub(crate) fn arc(&self) -> Arc<HostImpl> {
        self.me.upgrade().expect("a host method runs while the host is alive")
    }

    pub(crate) fn core(&self) -> Arc<dyn HostCore> {
        self.arc()
    }

    /// Whether an identity is the host's own: the same key as the host's identity.
    pub(crate) fn is_self(&self, identity: &Json) -> bool {
        identity.same_ref(&self.principal) || (self.principal_key)(identity) == self.self_key
    }

    pub(crate) fn principal_part(&self, identity: &Json) -> String {
        if self.is_self(identity) { "host".into() } else { format!("p:{}", (self.principal_key)(identity)) }
    }

    /// A commit failed midway: from now on this host admits no work (a reload seam).
    #[allow(dead_code, reason = "reload commits (phase 4) fail the host")]
    pub(crate) fn fail(&self, error: ChordError) {
        lock(&self.failed).get_or_insert_with(|| error.clone());
        self.report(error);
    }
}

impl HostCore for HostImpl {
    fn id(&self) -> u64 {
        self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn dev(&self) -> bool {
        self.dev
    }

    fn principal(&self) -> &Json {
        &self.principal
    }

    fn handle(&self) -> Option<&Handle> {
        self.handle.as_ref()
    }

    fn failed(&self) -> Option<ChordError> {
        lock(&self.failed).clone()
    }

    fn report(&self, error: ChordError) {
        (self.on_error)(error);
    }

    fn current_code_version(&self) -> i64 {
        self.state().current
    }

    fn has_code_version(&self, code_version: i64) -> bool {
        self.state().code_versions.contains_key(&code_version)
    }

    fn retained_services(&self, type_name: &str) -> Vec<(i64, Option<Arc<Service>>)> {
        let state = self.state();
        let versions = state.code_versions.values().chain(state.pending.iter());
        versions.map(|version| (version.id, version.services.get(type_name).cloned())).collect()
    }

    fn pending_code_version(&self) -> Option<i64> {
        self.state().pending.as_ref().map(|pending| pending.id)
    }

    /// The current code for a type; a type a running reload adds is served from the pending code version.
    fn service(&self, type_name: &str) -> Option<Arc<Service>> {
        let state = self.state();
        let current = state.code_versions.get(&state.current).and_then(|current| current.services.get(type_name));
        current.or_else(|| state.pending.as_ref().and_then(|pending| pending.services.get(type_name))).cloned()
    }

    fn acquire(
        &self,
        contract: &Contract,
        input: &Json,
        options: &GetOptions,
        owner: RefOwner,
        from: Option<&Arc<ObjectImpl>>,
        candidate: bool,
    ) -> ChordResult<Ref> {
        self.acquire_ref(contract, input, options, owner, from, candidate)
    }

    fn address_of(&self, contract: &Contract, input: &Json, principal: &Json, candidate: bool) -> ChordResult<String> {
        self.address(contract, input, principal, candidate)
    }

    fn root_erased(&self, run: RootFn, options: RootOptions) -> BoxFuture<ChordResult<()>> {
        self.start_root(run, options)
    }

    fn touch(&self, root_id: &str, object: &Arc<ObjectImpl>) {
        let mut state = self.state();
        let touched = state.touches.entry(root_id.to_string()).or_default();
        if !touched.iter().any(|existing| std::ptr::eq(existing.as_ptr(), Arc::as_ptr(object))) {
            touched.push(Arc::downgrade(object));
        }
    }

    fn forget(&self, object: &ObjectImpl) {
        self.forget_object(object);
    }

    fn disposed(&self, object: &ObjectImpl) {
        let mut state = self.state();
        if state.dying.get(object.address()).is_some_and(|dying| std::ptr::eq(Arc::as_ptr(dying), object)) {
            state.dying.remove(object.address());
        }
    }

    fn task_stop_deadline(&self) -> Duration {
        self.task_stop_deadline
    }

    fn ready_timeout(&self) -> Option<Duration> {
        self.ready_timeout
    }

    fn check_registrations(&self, object: &Arc<ObjectImpl>, decls: &Decls) -> ChordResult<()> {
        self.check_registered(object, decls)
    }

    fn registries_changed(&self, including: Option<&Arc<ObjectImpl>>) {
        self.rebuild_tables(including);
    }
}

impl Host {
    pub fn name(&self) -> &str {
        &self.0.name
    }

    /// The identity of the host's own code.
    pub fn principal(&self) -> &Json {
        &self.0.principal
    }

    pub fn code_version(&self) -> i64 {
        self.0.state().current
    }

    /// Registers a factory for a type. A `lifetime: "host"` type's object starts at once.
    pub fn serve<F>(&self, contract: &Contract, factory: F, options: ServeOptions) -> ChordResult<()>
    where
        F: Fn(&Json, &Json, &mut Declare) -> ChordResult<Implementation> + Send + Sync + 'static,
    {
        self.0.serve(contract, Arc::new(factory), options)
    }

    /// A reference held until `release`, possibly still loading.
    pub fn hold(&self, contract: &Contract, input: Json, options: GetOptions) -> ChordResult<Ref> {
        self.0.acquire_ref(contract, &input, &options, RefOwner::External, None, false)
    }

    /// A reference, once ready. On failure (activation error, `aborted`, `unavailable/ready_timeout`) the
    /// reference is released.
    pub fn get(&self, contract: &Contract, input: Json, options: GetOptions) -> BoxFuture<ChordResult<Ref>> {
        self.0.get(contract, input, options)
    }

    /// Runs `run` in a new root of this host.
    pub fn root<T, F, Fut>(&self, run: F, options: RootOptions) -> BoxFuture<ChordResult<T>>
    where
        T: Send + 'static,
        F: FnOnce(Ctx) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ChordResult<T>> + Send + 'static,
    {
        super::object::run_root(&self.0.core(), options, run)
    }

    /// The addresses of the objects this host has (diagnostics and tests).
    pub fn addresses(&self) -> Vec<String> {
        self.0.state().objects.keys().cloned().collect()
    }
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Host").field("name", &self.0.name).finish()
    }
}
