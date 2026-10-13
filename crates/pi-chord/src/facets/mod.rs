//! Facets: units that declare service needs and provisions, composed and reloaded by a host.

mod environment;
mod graph;
mod kernel;
mod lifecycle;
mod loader;
mod reload;
mod slots;
mod spawner;

use std::sync::{Arc, Mutex};

pub use environment::FacetEnvironment;
pub use lifecycle::{ActivateCallback, Disposal};
pub use loader::{FacetLoader, LoadedFacets, combine_facet_loaders, create_static_facet_loader};
pub use spawner::{LocalServiceSpawner, ServiceSpawner};

use self::kernel::{FacetKernel, KernelState, Phase, aggregate, check_ids};
use self::slots::HostServiceSlots;
use crate::error::{ChordError, ChordResult};
use crate::services::{RemoteServiceProvider, RemoteServiceSource};
use crate::state::ErrorReporter;

/// One unit of composition. `setup` runs synchronously and only declares; work starts on activation.
pub trait Facet: Send + Sync {
    fn id(&self) -> &str;
    fn setup(&self, env: &FacetEnvironment) -> ChordResult<()>;
}

struct FnFacet<F> {
    id: String,
    setup: F,
}

impl<F: Fn(&FacetEnvironment) -> ChordResult<()> + Send + Sync> Facet for FnFacet<F> {
    fn id(&self) -> &str {
        &self.id
    }

    fn setup(&self, env: &FacetEnvironment) -> ChordResult<()> {
        (self.setup)(env)
    }
}

/// Define a facet from its ID and setup function.
pub fn define_facet<F>(id: impl Into<String>, setup: F) -> Arc<dyn Facet>
where
    F: Fn(&FacetEnvironment) -> ChordResult<()> + Send + Sync + 'static,
{
    Arc::new(FnFacet { id: id.into(), setup })
}

/// How to build a facet host.
#[derive(Clone)]
pub struct FacetOptions {
    pub facets: Vec<Arc<dyn Facet>>,
    pub service_sources: Vec<Arc<dyn RemoteServiceSource>>,
    /// Receives errors that have no caller. Defaults to dropping them, as upstream does.
    pub on_error: Option<ErrorReporter>,
}

impl FacetOptions {
    pub fn new(facets: Vec<Arc<dyn Facet>>) -> Self {
        Self { facets, service_sources: Vec::new(), on_error: None }
    }
}

/// An active generation of facets and the provider that publishes their remote services.
pub struct FacetHost {
    kernel: Arc<FacetKernel>,
    services: RemoteServiceProvider,
}

impl FacetHost {
    /// The provider of every remotely exposable service the facets provide.
    pub fn services(&self) -> &RemoteServiceProvider {
        &self.services
    }

    /// Activate and swap in facets with matching IDs, keeping consumer service handles connected.
    pub async fn reload(&self, facets: Vec<Arc<dyn Facet>>) -> ChordResult<()> {
        self.kernel.reload(facets).await
    }

    pub async fn dispose(&self) -> ChordResult<()> {
        self.kernel.dispose().await
    }
}

/// Create and activate a host for one complete set of facets.
pub async fn create_facet_host(options: FacetOptions) -> ChordResult<FacetHost> {
    check_ids(options.facets.iter().map(|facet| facet.id()), "Facet IDs must be unique within a generation")?;
    let kernel = Arc::new(FacetKernel {
        initial_facets: options.facets,
        sources: options.service_sources,
        on_error: options.on_error.unwrap_or_else(|| Arc::new(|_: ChordError| {})),
        slots: Arc::new(HostServiceSlots::default()),
        phase: Arc::new(Mutex::new(Phase::Setup)),
        state: Mutex::new(KernelState::default()),
    });
    if let Err(error) = kernel.activate_generation().await {
        let cleanup = kernel.terminate(Vec::new()).await;
        return Err(aggregate(error, cleanup, "Facet generation startup and cleanup failed"));
    }
    let services = kernel.provider()?;
    Ok(FacetHost { kernel, services })
}
