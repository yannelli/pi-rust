//! Serving types and facets: named bundles of `serve` calls, installed together.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use futures::future::try_join_all;
use indexmap::IndexMap;

use super::{Host, HostImpl, HostRegistry};
use crate::context::{AbortController, AbortSignal};
use crate::error::{ChordError, ChordResult};
use crate::host2::contract::{Contract, placed_on, placement_text};
use crate::host2::errors::ChordErrorCode;
use crate::host2::object::{
    Ctx, Declare, Factory, HostCore, Implementation, Ref, RefOwner, RootOptions, ServeOptions, Service, make_service,
    run_root,
};
use crate::host2::registry::Registry;
use crate::host2::types::{GetOptions, RefStatus};
use crate::json::Json;
use crate::sync::{BoxFuture, lock};

/// What a facet runs to serve its types.
pub type FacetSetup = Arc<dyn Fn(&FacetApi) -> ChordResult<()> + Send + Sync>;

/// A named facet. Reloading a facet replaces everything it served.
#[derive(Clone)]
pub struct Facet {
    name: String,
    uses: Vec<Contract>,
    setup: FacetSetup,
}

impl Facet {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Contract tokens this facet consumes; checked against providers at reload.
    pub fn uses(&self) -> &[Contract] {
        &self.uses
    }

    pub fn with_uses(mut self, uses: Vec<Contract>) -> Self {
        self.uses = uses;
        self
    }
}

/// A facet named `name` whose `setup` serves its types.
pub fn define_facet(name: &str, setup: impl Fn(&FacetApi) -> ChordResult<()> + Send + Sync + 'static) -> Facet {
    Facet { name: name.to_string(), uses: Vec::new(), setup: Arc::new(setup) }
}

/// What `reload` loads: a facet, or a module the host's loader turns into one.
#[derive(Clone)]
pub enum PluginRef {
    Facet(Facet),
    Module(String),
}

impl From<Facet> for PluginRef {
    fn from(facet: Facet) -> Self {
        Self::Facet(facet)
    }
}

impl From<&Facet> for PluginRef {
    fn from(facet: &Facet) -> Self {
        Self::Facet(facet.clone())
    }
}

impl From<&str> for PluginRef {
    fn from(module: &str) -> Self {
        Self::Module(module.to_string())
    }
}

impl From<String> for PluginRef {
    fn from(module: String) -> Self {
        Self::Module(module)
    }
}

/// Where a facet's `serve` puts its services: the current code version, or a reload's candidate table.
pub(crate) type ServeTarget = Arc<Mutex<IndexMap<String, Arc<Service>>>>;

/// `install` and `load` options.
#[derive(Clone, Copy, Debug)]
pub struct LoadOptions {
    /// Await the activation of the `lifetime: "host"` objects the facets serve (default true).
    pub await_host_objects: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self { await_host_objects: true }
    }
}

/// What a facet's `setup` gets; it may be kept and used after `setup` returns. Weak: factories keep it.
#[derive(Clone)]
pub struct FacetApi {
    host: Weak<HostImpl>,
    name: String,
    facet: String,
    signal: AbortSignal,
    added: Arc<Mutex<Vec<Arc<Service>>>>,
    target: Option<ServeTarget>,
}

impl FacetApi {
    fn host(&self) -> ChordResult<Arc<HostImpl>> {
        let gone = || ChordError::code_only(ChordErrorCode::Gone, format!("host {} is gone", self.name));
        self.host.upgrade().ok_or_else(gone)
    }

    pub fn serve<F>(&self, contract: &Contract, factory: F, options: ServeOptions) -> ChordResult<()>
    where
        F: Fn(&Json, &Json, &mut Declare) -> ChordResult<Implementation> + Send + Sync + 'static,
    {
        let service = make_service(contract, Arc::new(factory), options, &self.facet)?;
        let host = self.host()?;
        match &self.target {
            Some(target) => host.check_served(&lock(target), &service)?,
            None => host.serve_into(service.clone())?,
        }
        if let Some(target) = &self.target {
            lock(target).insert(service.contract.name().to_string(), service.clone());
        }
        lock(&self.added).push(service);
        Ok(())
    }

    /// The API a reload gives a facet it loads: `serve` fills the reload's candidate table.
    pub(crate) fn for_reload(host: &HostImpl, facet: &str, signal: AbortSignal, target: ServeTarget) -> Self {
        let (name, added) = (host.name.clone(), Arc::default());
        Self { host: host.me.clone(), name, facet: facet.to_string(), signal, added, target: Some(target) }
    }

    /// Runs `facet`'s setup with this API.
    pub(crate) fn setup(&self, facet: &Facet) -> ChordResult<()> {
        (facet.setup)(self)
    }

    /// The host's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Starts a root on the facet's host (in its current code version at call time).
    pub fn root<T, F, Fut>(&self, run: F, options: RootOptions) -> BoxFuture<ChordResult<T>>
    where
        T: Send + 'static,
        F: FnOnce(Ctx) -> Fut + Send + 'static,
        Fut: Future<Output = ChordResult<T>> + Send + 'static,
    {
        match self.host() {
            Ok(host) => run_root(&host.core(), options, run),
            Err(error) => Box::pin(std::future::ready(Err(error))),
        }
    }

    pub fn registry<V>(&self, registry: &Registry<V>) -> HostRegistry<V> {
        HostRegistry::new(self.host.clone(), registry)
    }

    /// Aborted when this run of the facet is no longer installed (replaced, removed, or the host closed).
    pub fn signal(&self) -> &AbortSignal {
        &self.signal
    }
}

fn ready_all(refs: Vec<Ref>) -> BoxFuture<ChordResult<()>> {
    Box::pin(async move {
        try_join_all(refs.iter().map(|reference| reference.ready())).await?;
        Ok(())
    })
}

impl HostImpl {
    pub(super) fn serve(&self, contract: &Contract, factory: Factory, options: ServeOptions) -> ChordResult<()> {
        if self.state().reloading {
            return Err(ChordError::error("host.serve during a reload; serve from a facet passed to reload()"));
        }
        let service = make_service(contract, factory, options, "")?;
        self.serve_into(service.clone())?;
        if service.host_lifetime() {
            self.start_host_object(&service)?;
        }
        Ok(())
    }

    pub(super) fn serve_into(&self, service: Arc<Service>) -> ChordResult<()> {
        let name = service.contract.name().to_string();
        let mut state = self.state();
        let current = state.current;
        let Some(version) = state.code_versions.get_mut(&current) else { return Ok(()) };
        self.check_served(&version.services, &service)?;
        version.services.insert(name, service);
        Ok(())
    }

    /// Refuses a second service of one type and a service placed on other hosts.
    fn check_served(&self, services: &IndexMap<String, Arc<Service>>, service: &Service) -> ChordResult<()> {
        let name = service.contract.name();
        if services.contains_key(name) {
            return Err(ChordError::error(format!("{name} is already served")));
        }
        if let Some(placement) = service.contract.description().host()
            && !placed_on(placement, &self.name)
        {
            let message = format!("{name} is declared for {}, but {} serves it", placement_text(placement), self.name);
            return Err(ChordError::type_error(message));
        }
        Ok(())
    }

    pub(crate) fn start_host_object(&self, service: &Service) -> ChordResult<Ref> {
        let options = GetOptions::default();
        let reference =
            self.acquire_ref(&service.contract, &Json::empty_object(), &options, RefOwner::Host, None, false)?;
        self.state().host_objects.insert(service.contract.name().to_string(), reference.clone());
        // Reported when the reference is gone before it was live (upstream `ref.ready.catch(report)`), ahead of
        // the code that awaits it.
        let (host, was_live) = (self.me.clone(), AtomicBool::new(reference.status() == RefStatus::Live));
        let _ = reference.on_status(Arc::new(move |status, error| match (status, error) {
            (RefStatus::Live, _) => was_live.store(true, Ordering::SeqCst),
            (RefStatus::Gone, Some(error)) if !was_live.swap(true, Ordering::SeqCst) => {
                if let Some(host) = host.upgrade() {
                    host.report(error.clone());
                }
            }
            _ => {}
        }));
        Ok(reference)
    }

    /// Installs synchronously; the future settles once this facet's host objects are ready.
    fn install(&self, facet: &Facet, options: LoadOptions) -> ChordResult<BoxFuture<ChordResult<()>>> {
        if self.state().reloading {
            return Err(ChordError::error("host.install during a reload"));
        }
        if self.state().facets.contains_key(&facet.name) {
            return Err(ChordError::error(format!("facet {} is already installed; use reload()", facet.name)));
        }
        let run = AbortController::new();
        let (host, name, signal) = (self.me.clone(), self.name.clone(), run.signal());
        let api = FacetApi { host, name, facet: facet.name.clone(), signal, added: Arc::default(), target: None };
        if let Err(error) = (facet.setup)(&api) {
            run.abort(Some(error.clone()));
            return Err(error);
        }
        self.state().facets.insert(facet.name.clone(), facet.clone());
        self.facet_run_changed(&facet.name, Some(run));
        let added: Vec<Arc<Service>> = lock(&api.added).clone();
        let mut refs = Vec::new();
        for service in added.iter().filter(|service| service.host_lifetime()) {
            refs.push(self.start_host_object(service)?);
        }
        Ok(if options.await_host_objects { ready_all(refs) } else { Box::pin(std::future::ready(Ok(()))) })
    }

    /// Turns plugin references into facets, loading modules through the host's loader.
    pub(crate) async fn resolve(&self, plugins: Vec<PluginRef>) -> ChordResult<Vec<Facet>> {
        let modules: Vec<String> = plugins
            .iter()
            .filter_map(|plugin| match plugin {
                PluginRef::Module(module) => Some(module.clone()),
                PluginRef::Facet(_) => None,
            })
            .collect();
        let mut loaded = Vec::new().into_iter();
        if !modules.is_empty() {
            let Some(loader) = self.loader.clone() else {
                return Err(ChordError::error("loading modules needs a loader (HostOptions::loader)"));
            };
            loaded = loader.load(&modules).await?.into_iter();
        }
        let facets = plugins.into_iter().map(|plugin| match plugin {
            PluginRef::Facet(facet) => Ok(facet),
            PluginRef::Module(module) => {
                loaded.next().ok_or_else(|| ChordError::error(format!("{module}: the loader returned no facet")))
            }
        });
        facets.collect()
    }

    /// The installed run of facet `name` is now `run` (or none): the run it replaces is aborted.
    pub(crate) fn facet_run_changed(&self, name: &str, run: Option<AbortController>) {
        let replaced = {
            let mut state = self.state();
            match run {
                Some(run) => state.facet_runs.insert(name.to_string(), run),
                None => state.facet_runs.remove(name),
            }
        };
        if let Some(replaced) = replaced {
            let message = format!("facet {name} was replaced or removed");
            replaced.abort(Some(ChordError::code_only(ChordErrorCode::Gone, message)));
        }
    }
}

impl Host {
    /// Installs `facet` synchronously, then settles once its host objects are ready (unless not awaited).
    pub fn install(&self, facet: &Facet, options: LoadOptions) -> BoxFuture<ChordResult<()>> {
        self.0.install(facet, options).unwrap_or_else(|error| Box::pin(std::future::ready(Err(error))))
    }

    /// Installs every facet before awaiting their host objects (a host object's factory may use a type a later
    /// facet serves).
    pub fn load(&self, facets: &[Facet], options: LoadOptions) -> BoxFuture<ChordResult<()>> {
        let mut pending = Vec::new();
        for facet in facets {
            match self.0.install(facet, options) {
                Ok(ready) => pending.push(ready),
                Err(error) => return Box::pin(std::future::ready(Err(error))),
            }
        }
        Box::pin(async move {
            try_join_all(pending).await?;
            Ok(())
        })
    }

    /// Settles once the host objects present at call time are ready.
    pub fn ready(&self) -> BoxFuture<ChordResult<()>> {
        ready_all(self.0.state().host_objects.values().cloned().collect())
    }

    /// This host's view of `registry`.
    pub fn registry<V>(&self, registry: &Registry<V>) -> HostRegistry<V> {
        self.0.registry(registry)
    }

    /// The host's own reference to its `lifetime: "host"` object of `type_name` (upstream `hostObjects`).
    pub fn host_object(&self, type_name: &str) -> Option<Ref> {
        self.0.state().host_objects.get(type_name).cloned()
    }
}
