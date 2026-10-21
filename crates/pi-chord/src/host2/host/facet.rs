//! Serving types and facets: named bundles of `serve` calls, installed together.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use futures::future::try_join_all;

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

/// A named facet. Reloading a facet (phase 4) replaces everything it served.
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
        self.host()?.serve_into(service.clone())?;
        lock(&self.added).push(service);
        Ok(())
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
        if version.services.contains_key(&name) {
            return Err(ChordError::error(format!("{name} is already served")));
        }
        if let Some(placement) = service.contract.description().host()
            && !placed_on(placement, &self.name)
        {
            let message = format!("{name} is declared for {}, but {} serves it", placement_text(placement), self.name);
            return Err(ChordError::type_error(message));
        }
        version.services.insert(name, service);
        Ok(())
    }

    fn start_host_object(&self, service: &Service) -> ChordResult<Ref> {
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
        let api = FacetApi { host, name, facet: facet.name.clone(), signal, added: Arc::default() };
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

    /// The installed run of facet `name` is now `run` (or none): the run it replaces is aborted.
    pub(super) fn facet_run_changed(&self, name: &str, run: Option<AbortController>) {
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
}
