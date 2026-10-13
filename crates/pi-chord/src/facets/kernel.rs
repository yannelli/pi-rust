//! The lifecycle and dependency kernel behind a facet host.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use super::Facet;
use super::environment::{FacetEnvironment, FacetRuntime};
use super::graph::{ExternalService, validate_facets};
use super::lifecycle::single_or_aggregate;
use super::slots::{HostServiceSlots, KeyedSource, LocalKeyedServiceRegistry};
use super::spawner::{FacetProvision, SingletonValue};
use crate::context::BACKGROUND_CONTEXT;
use crate::error::{ChordError, ChordResult};
use crate::services::handle::{AccessCheck, SlotTarget};
use crate::services::{
    RemoteServiceBindingOptions, RemoteServiceProvider, RemoteServiceSource, RemoteServiceSourceOpenOptions,
    RemoteServices, ServiceDefinition, ServiceMode, ServiceRef, create_loopback_service_transport,
    create_remote_service_binding,
};
use crate::state::ErrorReporter;
use crate::sync::lock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Phase {
    Setup,
    Assembling,
    Connecting,
    Activating,
    Active,
    Reloading,
    Disposing,
    Dead,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Setup => "setup",
            Self::Assembling => "assembling",
            Self::Connecting => "connecting",
            Self::Activating => "activating",
            Self::Active => "active",
            Self::Reloading => "reloading",
            Self::Disposing => "disposing",
            Self::Dead => "dead",
        })
    }
}

#[derive(Default)]
pub(crate) struct KernelState {
    pub facets: IndexMap<String, Arc<FacetRuntime>>,
    pub source_bindings: Vec<(usize, Arc<dyn RemoteServices>)>,
    pub activation_order: Vec<String>,
    pub provider: Option<RemoteServiceProvider>,
    pub internal: Option<Arc<dyn RemoteServices>>,
    pub local_keyed: Option<Arc<LocalKeyedServiceRegistry>>,
}

pub(crate) struct FacetKernel {
    pub initial_facets: Vec<Arc<dyn Facet>>,
    pub sources: Vec<Arc<dyn RemoteServiceSource>>,
    pub on_error: ErrorReporter,
    pub slots: Arc<HostServiceSlots>,
    pub phase: Arc<Mutex<Phase>>,
    pub state: Mutex<KernelState>,
}

pub(crate) fn check_ids<'a>(ids: impl Iterator<Item = &'a str>, duplicate: &str) -> ChordResult<()> {
    let mut seen = HashSet::new();
    for id in ids {
        if id.is_empty() {
            return Err(ChordError::error("Facet ID must not be empty"));
        }
        if !seen.insert(id) {
            return Err(ChordError::error(duplicate));
        }
    }
    Ok(())
}

pub(crate) async fn dispose_records(records: impl IntoIterator<Item = Arc<FacetRuntime>>) -> Vec<ChordError> {
    let mut errors = Vec::new();
    for record in records {
        if let Err(error) = record.lifecycle.dispose().await {
            errors.push(error);
        }
    }
    errors
}

pub(crate) fn aggregate(error: ChordError, mut cleanup: Vec<ChordError>, message: &str) -> ChordError {
    if cleanup.is_empty() {
        return error;
    }
    cleanup.insert(0, error);
    ChordError::Aggregate { message: message.to_string(), errors: cleanup }
}

impl FacetKernel {
    pub fn phase(&self) -> Phase {
        *lock(&self.phase)
    }

    pub fn set_phase(&self, phase: Phase) {
        *lock(&self.phase) = phase;
    }

    pub fn provider(&self) -> ChordResult<RemoteServiceProvider> {
        lock(&self.state).provider.clone().ok_or_else(|| ChordError::error("Facet service provider is not assembled"))
    }

    pub fn local_keyed(&self) -> ChordResult<Arc<LocalKeyedServiceRegistry>> {
        let registry = lock(&self.state).local_keyed.clone();
        registry.ok_or_else(|| ChordError::error("Facet keyed services are not assembled"))
    }

    fn internal(&self) -> ChordResult<Arc<dyn RemoteServices>> {
        let internal = lock(&self.state).internal.clone();
        internal.ok_or_else(|| ChordError::error("Facet remote services are not assembled"))
    }

    /// Facet service targets resolve only once the generation is connected.
    pub fn target_access(&self) -> AccessCheck {
        let phase = self.phase.clone();
        Arc::new(move || {
            let phase = *lock(&phase);
            match phase {
                Phase::Activating | Phase::Active | Phase::Reloading | Phase::Disposing => Ok(()),
                _ => Err(ChordError::error(format!("Facet service targets cannot be used during {phase}"))),
            }
        })
    }

    pub fn setup_facet(&self, facet: &Arc<dyn Facet>, record: &Arc<FacetRuntime>) -> ChordResult<()> {
        facet.setup(&FacetEnvironment::new(record.clone(), self.slots.clone()))?;
        record.lifecycle.prepared()
    }

    pub fn provisions(&self) -> Vec<FacetProvision> {
        let records: Vec<_> = lock(&self.state).facets.values().cloned().collect();
        records.iter().flat_map(|record| record.provisions()).collect()
    }

    pub async fn activate_generation(&self) -> ChordResult<()> {
        let mut records = Vec::new();
        for facet in &self.initial_facets {
            let record = FacetRuntime::new(facet.id());
            lock(&self.state).facets.insert(facet.id().to_string(), record.clone());
            self.setup_facet(facet, &record)?;
            records.push(record);
        }
        self.set_phase(Phase::Assembling);
        let external = self.resolve_external_services(&records).await?;
        let order = validate_facets(&records, &external)?;
        lock(&self.state).activation_order = order.clone();
        self.assemble_providers()?;
        self.bind_services(&external)?;

        self.set_phase(Phase::Connecting);
        let mut bindings: Vec<Arc<dyn RemoteServices>> =
            lock(&self.state).source_bindings.iter().map(|(_, binding)| binding.clone()).collect();
        bindings.push(self.internal()?);
        futures::future::try_join_all(bindings.iter().map(|binding| binding.ready(&BACKGROUND_CONTEXT))).await?;

        self.set_phase(Phase::Activating);
        for id in order {
            let record = lock(&self.state).facets.get(&id).cloned();
            if let Some(record) = record {
                record.lifecycle.activate().await?;
            }
        }
        self.set_phase(Phase::Active);
        Ok(())
    }

    async fn resolve_external_services(
        &self,
        records: &[Arc<FacetRuntime>],
    ) -> ChordResult<IndexMap<String, ExternalService>> {
        let catalogues =
            futures::future::try_join_all(self.sources.iter().map(|source| source.catalogue(&BACKGROUND_CONTEXT)))
                .await?;
        let mut offered: IndexMap<String, (ServiceMode, usize)> = IndexMap::new();
        for (index, entries) in catalogues.into_iter().enumerate() {
            for entry in entries {
                if offered.contains_key(&entry.service_id) {
                    let id = entry.service_id;
                    return Err(ChordError::error(format!(
                        "Facet host service {id} is offered by more than one source"
                    )));
                }
                offered.insert(entry.service_id, (entry.mode, index));
            }
        }
        let local: HashSet<String> = records
            .iter()
            .flat_map(|record| lock(&record.provides).iter().map(|p| p.service_id().to_string()).collect::<Vec<_>>())
            .collect();
        let deferred: Vec<usize> =
            (0..self.sources.len()).filter(|index| self.sources[*index].accepts_unavailable_services()).collect();
        let mut external: IndexMap<String, ExternalService> = IndexMap::new();
        for record in records {
            let needs = lock(&record.requires).clone();
            for needed in needs {
                let id = needed.service_id();
                if local.contains(id) || external.contains_key(id) {
                    continue;
                }
                let mut source = offered.get(id).copied();
                if source.is_none() {
                    if deferred.len() > 1 {
                        return Err(ChordError::error(format!(
                            "Facet host service {id} has more than one deferred source"
                        )));
                    }
                    source = deferred.first().map(|index| (needed.mode, *index));
                }
                if let Some((mode, source)) = source {
                    external.insert(id.to_string(), ExternalService { service: needed.service.clone(), mode, source });
                }
            }
        }
        let mut by_source: IndexMap<usize, Vec<ServiceRef>> = IndexMap::new();
        for service in external.values() {
            by_source.entry(service.source).or_default().push(service.service.clone());
        }
        for (index, services) in by_source {
            let options = RemoteServiceSourceOpenOptions {
                services,
                assert_access: self.target_access(),
                on_error: self.on_error.clone(),
            };
            let binding = self.sources[index].open(options)?;
            lock(&self.state).source_bindings.push((index, binding));
        }
        Ok(external)
    }

    fn assemble_providers(&self) -> ChordResult<()> {
        let provisions = self.provisions();
        let remote: Vec<&FacetProvision> = provisions.iter().filter(|p| !p.service().is_local()).collect();
        let definitions: Vec<ServiceDefinition> = remote
            .iter()
            .map(|p| {
                let mode = if p.is_keyed() { ServiceMode::Keyed } else { ServiceMode::Singleton };
                ServiceDefinition { service: p.service().clone(), mode }
            })
            .collect();
        let provider = RemoteServiceProvider::new(&definitions)?;
        let internal = create_remote_service_binding(RemoteServiceBindingOptions {
            on_error: Some(self.on_error.clone()),
            assert_access: Some(self.target_access()),
            ..RemoteServiceBindingOptions::new(
                remote.iter().map(|p| p.service().clone()).collect(),
                create_loopback_service_transport(&provider),
            )
        })?;
        let local_keyed: Vec<ServiceRef> =
            provisions.iter().filter(|p| p.is_keyed() && p.service().is_local()).map(|p| p.service().clone()).collect();
        let registry = LocalKeyedServiceRegistry::new(&local_keyed, self.on_error.clone())?;
        {
            let mut state = lock(&self.state);
            state.provider = Some(provider.clone());
            state.internal = Some(Arc::new(internal));
            state.local_keyed = Some(registry.clone());
        }
        for provision in &provisions {
            match provision {
                FacetProvision::Singleton { service, value: SingletonValue::Remote(implementation) } => {
                    provider.provide(service, implementation.clone())?;
                }
                FacetProvision::Singleton { .. } => {}
                FacetProvision::Keyed { service, .. } if service.is_local() => provision.connect_local(&registry)?,
                FacetProvision::Keyed { .. } => provision.connect_remote(&provider)?,
            }
        }
        Ok(())
    }

    fn bind_services(&self, external: &IndexMap<String, ExternalService>) -> ChordResult<()> {
        let internal = self.internal()?;
        for provision in self.provisions() {
            match &provision {
                FacetProvision::Singleton { service, value } => {
                    if !self.slots.has_singleton(service.id()) {
                        continue;
                    }
                    let target = match value {
                        SingletonValue::Local(implementation) => SlotTarget::Local(implementation.clone()),
                        SingletonValue::Remote(_) => SlotTarget::Remote(internal.use_service(service)?),
                    };
                    self.slots.bind_singleton(service.id(), target);
                }
                FacetProvision::Keyed { service, .. } => {
                    let source = if service.is_local() {
                        KeyedSource::Local(self.local_keyed()?)
                    } else {
                        KeyedSource::Remote(internal.clone())
                    };
                    self.slots.bind_keyed(service.id(), source);
                }
            }
        }
        for (id, service) in external {
            let binding = lock(&self.state)
                .source_bindings
                .iter()
                .find(|(index, _)| *index == service.source)
                .map(|(_, binding)| binding.clone())
                .ok_or_else(|| ChordError::error(format!("Service source for {id} is not open")))?;
            match service.mode {
                ServiceMode::Singleton => {
                    let handle = binding.use_service(&service.service)?;
                    self.slots.bind_singleton(id, SlotTarget::Remote(handle));
                }
                ServiceMode::Keyed => self.slots.bind_keyed(id, KeyedSource::Remote(binding)),
            }
        }
        Ok(())
    }

    pub async fn abort(&self, extra: Vec<Arc<FacetRuntime>>) -> Vec<ChordError> {
        let records: Vec<_> = lock(&self.state).facets.values().cloned().collect();
        for record in records.iter().chain(extra.iter()) {
            record.lifecycle.revoke();
        }
        self.terminate(extra).await
    }

    pub async fn terminate(&self, extra: Vec<Arc<FacetRuntime>>) -> Vec<ChordError> {
        self.set_phase(Phase::Disposing);
        let mut errors = self.dispose_lifecycles().await;
        errors.extend(dispose_records(extra.into_iter().rev()).await);
        let registry = lock(&self.state).local_keyed.take();
        if let Some(registry) = registry {
            registry.dispose();
        }
        errors.extend(self.dispose_service_bindings().await);
        self.slots.dispose();
        let provider = lock(&self.state).provider.clone();
        if let Some(provider) = provider
            && let Err(error) = provider.dispose()
        {
            errors.push(error);
        }
        self.set_phase(Phase::Dead);
        errors
    }

    async fn dispose_lifecycles(&self) -> Vec<ChordError> {
        let order: Vec<String> = {
            let state = lock(&self.state);
            if state.activation_order.is_empty() {
                state.facets.keys().rev().cloned().collect()
            } else {
                state.activation_order.iter().rev().cloned().collect()
            }
        };
        let mut errors = Vec::new();
        for id in order {
            let record = lock(&self.state).facets.shift_remove(&id);
            if let Some(record) = record
                && let Err(error) = record.lifecycle.dispose().await
            {
                errors.push(error);
            }
        }
        errors
    }

    async fn dispose_service_bindings(&self) -> Vec<ChordError> {
        let bindings: Vec<Arc<dyn RemoteServices>> = {
            let mut state = lock(&self.state);
            let mut bindings: Vec<_> = state.source_bindings.drain(..).map(|(_, binding)| binding).collect();
            bindings.extend(state.internal.take());
            bindings
        };
        let results =
            futures::future::join_all(bindings.iter().map(|binding| binding.dispose(&BACKGROUND_CONTEXT))).await;
        results.into_iter().filter_map(Result::err).collect()
    }

    pub async fn dispose(&self) -> ChordResult<()> {
        match self.phase() {
            Phase::Dead => return Ok(()),
            Phase::Active => {}
            phase => return Err(ChordError::error(format!("Facet host cannot be disposed while {phase}"))),
        }
        single_or_aggregate(self.terminate(Vec::new()).await, "Failed to dispose facet generation")
    }
}
