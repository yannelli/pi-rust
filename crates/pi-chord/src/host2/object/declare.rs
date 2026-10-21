//! [`Declare`], upstream's `Self`: what a factory declares through. One factory run records its declarations
//! in [`Decls`]; [`Run`] adds the objects it created (not reused from an earlier run).

use std::any::Any;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use indexmap::IndexMap;

use super::resource::{AnyValue, ErasedDeclaration, Exposed};
use super::{Ctx, Implementation, Member, ObjectImpl, Ref, RefOwner, ResourceHandle, ResourceImpl};
use crate::error::{ChordError, ChordResult};
use crate::host2::cell::{Cell, DeriveFn, Derived, SourceOptions};
use crate::host2::contract::Contract;
use crate::host2::declare::{EventSource, ResourceDeclaration};
use crate::host2::errors::invalid;
use crate::host2::registry::{Registration, Registry};
use crate::host2::types::{GetOptions, ReadableRef, readable_id};
use crate::json::Json;
use crate::sync::{BoxFuture, lock};

/// A cell's first value from its source, run in a root of its own before the object goes live.
pub type InitFn = Arc<dyn Fn(Ctx) -> BoxFuture<ChordResult<Json>> + Send + Sync>;
/// Maps the value of an older version (`from`) to this version's, at the reload that raised the version.
pub type Migrate = Arc<dyn Fn(Json, i64) -> ChordResult<Json> + Send + Sync>;
/// Long-running work of the live object.
pub type TaskFn = Arc<dyn Fn(Ctx) -> BoxFuture<ChordResult<()>> + Send + Sync>;
/// What a listener runs for each event, in a fresh root.
pub(crate) type Handler = Arc<dyn Fn(Json, Ctx) -> BoxFuture<ChordResult<()>> + Send + Sync>;
type SelectFn = Arc<dyn Fn(&AnyValue) -> ChordResult<Listenable> + Send + Sync>;

/// A cell's first value: given, or read by `init` before the object goes live.
#[derive(Clone)]
pub enum Initial {
    Value(Json),
    Init(InitFn),
}

/// Options of `Declare::state_with`.
#[derive(Clone, Default)]
pub struct StateOptions {
    /// The meaning of the cell's value (default 1). A reload that raises it runs `migrate`.
    pub version: Option<i64>,
    pub migrate: Option<Migrate>,
}

/// What a listener follows: a readable's values, an event source's events, or a resource whose value is one.
#[derive(Clone)]
pub enum Listen {
    State(ReadableRef),
    Events(EventSource),
    Resource(Arc<ResourceImpl>),
}

impl From<ReadableRef> for Listen {
    fn from(readable: ReadableRef) -> Self {
        Self::State(readable)
    }
}

impl From<&Arc<Cell>> for Listen {
    fn from(cell: &Arc<Cell>) -> Self {
        Self::State(cell.clone())
    }
}

impl From<&Arc<Derived>> for Listen {
    fn from(derived: &Arc<Derived>) -> Self {
        Self::State(derived.clone())
    }
}

impl From<EventSource> for Listen {
    fn from(source: EventSource) -> Self {
        Self::Events(source)
    }
}

impl<T> From<&ResourceHandle<T>> for Listen {
    fn from(resource: &ResourceHandle<T>) -> Self {
        Self::Resource(resource.inner().clone())
    }
}

/// What a listener subscribes to once its source is resolved.
#[derive(Clone)]
pub enum Listenable {
    State(ReadableRef),
    Events(EventSource),
}

/// `select` with the key that stands for its identity (upstream compares the function's source text).
#[derive(Clone)]
pub(crate) struct Select {
    pub key: String,
    pub select: SelectFn,
}

#[derive(Clone)]
pub(crate) struct ListenerDecl {
    pub source: Listen,
    pub select: Option<Select>,
    pub handler: Handler,
}

impl ListenerDecl {
    /// The resource a listener is declared on, or its source itself.
    pub(crate) fn origin(&self) -> usize {
        match &self.source {
            Listen::State(readable) => readable_id(readable),
            Listen::Events(source) => Arc::as_ptr(source) as *const () as usize,
            Listen::Resource(resource) => Arc::as_ptr(resource) as usize,
        }
    }

    pub(crate) fn select_key(&self) -> Option<&str> {
        self.select.as_ref().map(|select| select.key.as_str())
    }
}

/// Everything one factory run declared, by kind and key.
#[derive(Default)]
pub(crate) struct DeclsData {
    pub cells: IndexMap<String, Arc<Cell>>,
    pub cell_schemas: IndexMap<String, Json>,
    pub cell_versions: IndexMap<String, i64>,
    pub cell_migrations: IndexMap<String, Migrate>,
    /// By address.
    pub refs: IndexMap<String, Ref>,
    pub resources: IndexMap<String, Arc<ResourceImpl>>,
    /// Resources kept from the earlier run, with this run's declarations (adopted at commit).
    pub adopted: Vec<(Arc<ResourceImpl>, ErasedDeclaration)>,
    pub listeners: IndexMap<String, ListenerDecl>,
    /// By position (`#0`, `#1`, ...): tasks have no names.
    pub tasks: IndexMap<String, TaskFn>,
    /// By position: derivations have no names and are new at every run.
    pub derives: IndexMap<String, Arc<Derived>>,
    pub registrations: IndexMap<String, Registration>,
}

/// A run's declarations; identity matters (`Arc::ptr_eq`), the contents are filled while the factory runs.
#[derive(Default)]
pub struct Decls {
    data: Mutex<DeclsData>,
}

impl Decls {
    pub(crate) fn data(&self) -> MutexGuard<'_, DeclsData> {
        lock(&self.data)
    }
}

/// The objects one run created.
#[derive(Default)]
pub(crate) struct Created {
    pub refs: Vec<Ref>,
    pub resources: Vec<Arc<ResourceImpl>>,
    pub derives: Vec<Arc<Derived>>,
    /// Cells this run created with `init`, which fills them before they are used.
    pub inits: Vec<(Arc<Cell>, InitFn)>,
}

/// One factory run: its implementation, declarations and the objects it created.
pub(crate) struct Run {
    pub implementation: Arc<Implementation>,
    pub decls: Arc<Decls>,
    pub created: Created,
}

/// What a factory declares through (upstream's `Self`).
pub struct Declare {
    object: Arc<ObjectImpl>,
    earlier: Option<Arc<Decls>>,
    decls: Arc<Decls>,
    created: Created,
}

impl Declare {
    pub(crate) fn new(object: Arc<ObjectImpl>, earlier: Option<Arc<Decls>>) -> Self {
        Self { object, earlier, decls: Arc::default(), created: Created::default() }
    }

    pub(crate) fn decls(&self) -> &Arc<Decls> {
        &self.decls
    }

    /// What the run declared and created so far (a factory that fails is unwound from it).
    pub(crate) fn into_run(self, implementation: Implementation) -> Run {
        Run { implementation: Arc::new(implementation), decls: self.decls, created: self.created }
    }

    fn source_options(&self) -> SourceOptions {
        let lock = self.object.write_lock.clone();
        SourceOptions { dev: self.object.host().dev(), report: self.object.report(), lock }
    }

    /// Refuses a second declaration of `name` of one kind in this run.
    fn unique(&self, exists: bool, kind: &str, name: &str) -> ChordResult<()> {
        if name.is_empty() {
            return Err(ChordError::type_error(format!("{kind} needs a name")));
        }
        if exists {
            let message = format!("{}: duplicate {kind} name {name}", self.object.type_name());
            return Err(invalid("duplicate", message, None));
        }
        Ok(())
    }

    /// A state cell named `name` with its schema and first value. A reload keeps the cell and its value.
    pub fn state(&mut self, name: &str, schema: Json, value: Json) -> ChordResult<Arc<Cell>> {
        self.state_with(name, schema, Initial::Value(value), StateOptions::default())
    }

    /// A state cell whose first value `init` reads from its source before the object goes live.
    pub fn state_init<F, Fut>(&mut self, name: &str, schema: Json, init: F) -> ChordResult<Arc<Cell>>
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChordResult<Json>> + Send + 'static,
    {
        let init: InitFn = Arc::new(move |ctx| Box::pin(init(ctx)));
        self.state_with(name, schema, Initial::Init(init), StateOptions::default())
    }

    pub fn state_with(
        &mut self,
        name: &str,
        schema: Json,
        first: Initial,
        options: StateOptions,
    ) -> ChordResult<Arc<Cell>> {
        self.unique(self.decls.data().cells.contains_key(name), "state", name)?;
        let version = options.version.unwrap_or(1);
        if version < 1 {
            return Err(ChordError::type_error(format!("state {name}: version must be a positive integer")));
        }
        let kept = self.earlier.as_ref().and_then(|earlier| earlier.data().cells.get(name).cloned());
        let cell = match kept {
            Some(cell) => cell,
            None => {
                let (value, init) = match first {
                    Initial::Value(value) => (Some(value), None),
                    Initial::Init(init) => (None, Some(init)),
                };
                let check = self.object.write_check();
                let cell = Cell::new(name, schema.clone(), value, check, self.source_options())?;
                if let Some(init) = init {
                    self.created.inits.push((cell.clone(), init));
                }
                cell
            }
        };
        let mut data = self.decls.data();
        data.cells.insert(name.to_string(), cell.clone());
        data.cell_schemas.insert(name.to_string(), schema);
        data.cell_versions.insert(name.to_string(), version);
        if let Some(migrate) = options.migrate {
            data.cell_migrations.insert(name.to_string(), migrate);
        }
        Ok(cell)
    }

    /// An object-owned reference, identified by its address.
    pub fn get(&mut self, contract: &Contract, input: Json, options: GetOptions) -> ChordResult<Ref> {
        let host = self.object.host().clone();
        // A reload's rerun binds new references against the pending contracts (consumer and provider reload together).
        let candidate = self.earlier.is_some();
        let principal = options.principal.clone().unwrap_or_else(|| host.principal().clone());
        let address = host.address_of(contract, &input, &principal, candidate)?;
        let declared = self.decls.data().refs.get(&address).cloned();
        if let Some(declared) = declared {
            if declared.contract().fingerprint() != contract.fingerprint() {
                let type_name = self.object.type_name();
                let message = format!("{type_name}: {} held twice with different contracts", contract.name());
                return Err(invalid("duplicate", message, None));
            }
            return Ok(declared);
        }
        let existing = self.earlier.as_ref().and_then(|earlier| earlier.data().refs.get(&address).cloned());
        if let Some(existing) = existing
            && existing.contract().fingerprint() == contract.fingerprint()
            && !existing.status().ended()
        {
            self.decls.data().refs.insert(address, existing.clone());
            return Ok(existing);
        }
        let reference = host.acquire(contract, &input, &options, RefOwner::Object, Some(&self.object), candidate)?;
        self.decls.data().refs.insert(address, reference.clone());
        self.created.refs.push(reference.clone());
        Ok(reference)
    }

    /// A resource opened before the object goes live and closed after it is disposed. A reload keeps it unless
    /// its version, dependencies or exclusivity changed.
    pub fn resource<T: Clone + Send + Sync + 'static>(
        &mut self,
        name: &str,
        declaration: ResourceDeclaration<T>,
    ) -> ChordResult<ResourceHandle<T>> {
        self.unique(self.decls.data().resources.contains_key(name), "resource", name)?;
        let deps = declaration.deps.clone();
        let spec = ErasedDeclaration::from(declaration);
        let existing = self.earlier.as_ref().and_then(|earlier| earlier.data().resources.get(name).cloned());
        let kept = existing.filter(|existing| {
            existing.version() == spec.version && existing.exclusive() == spec.exclusive && existing.deps == deps
        });
        let resource = match kept {
            Some(existing) => {
                self.decls.data().adopted.push((existing.clone(), spec));
                existing
            }
            None => {
                let resource = ResourceImpl::new(name, spec, deps);
                self.created.resources.push(resource.clone());
                resource
            }
        };
        self.decls.data().resources.insert(name.to_string(), resource.clone());
        Ok(ResourceHandle::new(resource, self.object.host().id()))
    }

    /// Reacts to `source` once the object is live; each event runs `handler` in a fresh root.
    pub fn on<F, Fut>(&mut self, name: &str, source: impl Into<Listen>, handler: F) -> ChordResult<()>
    where
        F: Fn(Json, Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChordResult<()>> + Send + 'static,
    {
        self.listener(name, source.into(), None, handler)
    }

    /// Like [`Declare::on`], subscribing to what `select` makes of the resource's opened value. `key` stands
    /// for the identity of `select`: a reload keeps the subscription while resource and key are unchanged.
    pub fn on_select<T, S, F, Fut>(
        &mut self,
        name: &str,
        resource: &ResourceHandle<T>,
        key: &str,
        select: S,
        handler: F,
    ) -> ChordResult<()>
    where
        T: Any + Send + Sync,
        S: Fn(&T) -> ChordResult<Listenable> + Send + Sync + 'static,
        F: Fn(Json, Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChordResult<()>> + Send + 'static,
    {
        let resource_key = resource.key().to_string();
        let select: SelectFn = Arc::new(move |value: &AnyValue| match value.downcast_ref::<T>() {
            Some(value) => select(value),
            None => Err(ChordError::type_error(format!("resource {resource_key} holds another type"))),
        });
        let select = Select { key: key.to_string(), select };
        self.listener(name, Listen::Resource(resource.inner().clone()), Some(select), handler)
    }

    fn listener<F, Fut>(&mut self, name: &str, source: Listen, select: Option<Select>, handler: F) -> ChordResult<()>
    where
        F: Fn(Json, Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChordResult<()>> + Send + 'static,
    {
        self.unique(self.decls.data().listeners.contains_key(name), "listener", name)?;
        let handler: Handler = Arc::new(move |event, ctx| Box::pin(handler(event, ctx)));
        self.decls.data().listeners.insert(name.to_string(), ListenerDecl { source, select, handler });
        Ok(())
    }

    /// Long-running work of the live object (a code holder: its calls start fresh roots).
    pub fn task<F, Fut>(&mut self, task: F)
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChordResult<()>> + Send + 'static,
    {
        let mut data = self.decls.data();
        let key = format!("#{}", data.tasks.len());
        data.tasks.insert(key, Arc::new(move |ctx| Box::pin(task(ctx))));
    }

    /// A synchronous projection of readables, recomputed when they change; a new one at every reload.
    pub fn derive<F>(&mut self, sources: Vec<ReadableRef>, compute: F) -> Arc<Derived>
    where
        F: Fn(&[Json]) -> ChordResult<Json> + Send + Sync + 'static,
    {
        let key = format!("#{}", self.decls.data().derives.len());
        let compute: DeriveFn = Arc::new(compute);
        let derived = Derived::new(key.clone(), sources, compute, self.source_options());
        self.created.derives.push(derived.clone());
        self.decls.data().derives.insert(key, derived.clone());
        derived
    }

    /// Objects of lifetime `Host` only. Keys are the registry's own keys.
    pub fn register<V: Any + Send + Sync>(
        &mut self,
        registry: &Registry<V>,
        entries: impl IntoIterator<Item = (String, V)>,
    ) -> ChordResult<()> {
        if !self.object.host_lifetime() {
            let message = format!("{}: only objects of lifetime \"host\" may register", self.object.type_name());
            return Err(invalid("not_host_lifetime", message, None));
        }
        for (key, value) in entries {
            let id = format!("{}\u{0}{key}", registry.name());
            self.unique(self.decls.data().registrations.contains_key(&id), "register", &id)?;
            self.decls.data().registrations.insert(id, Registration::new(registry, key, value));
        }
        Ok(())
    }

    /// A member backed by one of this object's resources: references read its value once the object is live.
    pub fn expose<T>(&self, resource: &ResourceHandle<T>) -> ChordResult<Member> {
        let declared = self.decls.data().resources.values().any(|declared| Arc::ptr_eq(declared, resource.inner()));
        if !declared {
            return Err(ChordError::type_error("expose takes a resource declared by this object"));
        }
        Ok(Member::Opaque(Arc::new(Exposed { resource: resource.inner().clone() })))
    }
}
