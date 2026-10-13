//! The facet setup environment: declare dependencies and provisions, own resources.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use super::lifecycle::{FacetLifecycle, ObservationStart};
use super::slots::{HostServiceSlots, LocalObserveHandler};
use super::spawner::{FacetProvision, LocalServiceSpawner, ServiceSpawner, SingletonValue, StagedServiceSpawner};
use crate::context::Context;
use crate::error::{ChordError, ChordResult};
use crate::services::handle::{AccessCheck, LocalHandle, ServiceSlot, local_impl};
use crate::services::{ObserveHandler, Service, ServiceHandle, ServiceImpl, ServiceMode, ServiceRef};
use crate::state::MutableReplicatedState;
use crate::sync::{Unsubscribe, lock};
use crate::{Json, replicated_state};

#[derive(Clone)]
pub(crate) struct FacetServiceReference {
    pub service: ServiceRef,
    pub mode: ServiceMode,
}

impl FacetServiceReference {
    pub fn service_id(&self) -> &str {
        self.service.id()
    }
}

/// The host's record of one facet in one generation.
pub(crate) struct FacetRuntime {
    pub facet_id: String,
    pub requires: Mutex<Vec<FacetServiceReference>>,
    pub provides: Mutex<Vec<FacetServiceReference>>,
    pub lifecycle: Arc<FacetLifecycle>,
    pub provisions: Mutex<Vec<FacetProvision>>,
    singleton_views: Mutex<HashMap<String, Box<dyn Any + Send + Sync>>>,
}

impl FacetRuntime {
    pub fn new(facet_id: &str) -> Arc<Self> {
        Arc::new(Self {
            facet_id: facet_id.to_string(),
            requires: Mutex::default(),
            provides: Mutex::default(),
            lifecycle: Arc::new(FacetLifecycle::new(facet_id)),
            provisions: Mutex::default(),
            singleton_views: Mutex::default(),
        })
    }

    pub fn provisions(&self) -> Vec<FacetProvision> {
        lock(&self.provisions).clone()
    }
}

fn record(target: &Mutex<Vec<FacetServiceReference>>, service: &ServiceRef, mode: ServiceMode) {
    let mut target = lock(target);
    if !target.iter().any(|reference| reference.service_id() == service.id() && reference.mode == mode) {
        target.push(FacetServiceReference { service: service.clone(), mode });
    }
}

fn require_remote(service: &ServiceRef) -> ChordResult<()> {
    if service.is_local() {
        return Err(ChordError::type_error(format!("Service {} is process-local", service.id())));
    }
    Ok(())
}

fn require_local(service: &ServiceRef) -> ChordResult<()> {
    if !service.is_local() {
        return Err(ChordError::type_error(format!("Service {} is remotely exposable", service.id())));
    }
    Ok(())
}

/// What a facet's `setup` receives. Clones share the facet; activation callbacks may keep one.
#[derive(Clone)]
pub struct FacetEnvironment {
    runtime: Arc<FacetRuntime>,
    slots: Arc<HostServiceSlots>,
}

impl FacetEnvironment {
    pub(crate) fn new(runtime: Arc<FacetRuntime>, slots: Arc<HostServiceSlots>) -> Self {
        Self { runtime, slots }
    }

    fn lifecycle(&self) -> &Arc<FacetLifecycle> {
        &self.runtime.lifecycle
    }

    fn access(&self) -> AccessCheck {
        let lifecycle = self.lifecycle().clone();
        Arc::new(move || lifecycle.assert_service_access())
    }

    fn cached_view<V: Clone + Send + Sync + 'static>(
        &self,
        service: &ServiceRef,
        create: impl FnOnce() -> V,
    ) -> ChordResult<V> {
        let mut views = lock(&self.runtime.singleton_views);
        if let Some(view) = views.get(service.id()) {
            let message = || ChordError::type_error(format!("Service {} was used with another type", service.id()));
            return view.downcast_ref::<V>().cloned().ok_or_else(message);
        }
        let view = create();
        views.insert(service.id().to_string(), Box::new(view.clone()));
        Ok(view)
    }

    /// Declare a hard dependency on one remotely exposable singleton and return its stable handle.
    pub fn use_service(&self, service: &ServiceRef) -> ChordResult<ServiceHandle> {
        self.lifecycle().assert_setting_up("acquire services")?;
        require_remote(service)?;
        record(&self.runtime.requires, service, ServiceMode::Singleton);
        self.cached_view(service, || self.slots.remote_singleton(service, self.access()))
    }

    /// Declare a hard dependency on one process-local singleton and return its stable handle.
    pub fn use_local<T: ?Sized + Send + Sync + 'static>(&self, service: &Service<T>) -> ChordResult<LocalHandle<T>> {
        self.lifecycle().assert_setting_up("acquire services")?;
        require_local(service)?;
        record(&self.runtime.requires, service, ServiceMode::Singleton);
        self.cached_view(service, || self.slots.local_singleton::<T>(service, self.access()))
    }

    /// Declare a hard dependency on a remotely exposable keyed service and watch each live instance.
    pub fn observe(&self, service: &ServiceRef, handler: ObserveHandler) -> ChordResult<()> {
        self.lifecycle().assert_setting_up("observe services")?;
        require_remote(service)?;
        record(&self.runtime.requires, service, ServiceMode::Keyed);
        let (slots, service, check) = (self.slots.clone(), service.clone(), self.access());
        let start: ObservationStart = Box::new(move || {
            let stop = slots.observe_remote(&service, check, handler)?;
            Ok(disposal_of(stop))
        });
        self.lifecycle().observe(start)
    }

    /// Declare a hard dependency on a process-local keyed service and watch each live instance.
    pub fn observe_local<T, F, Fut>(&self, service: &Service<T>, handler: F) -> ChordResult<()>
    where
        T: ?Sized + Send + Sync + 'static,
        F: Fn(LocalHandle<T>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChordResult<()>> + Send + 'static,
    {
        self.lifecycle().assert_setting_up("observe services")?;
        require_local(service)?;
        record(&self.runtime.requires, service, ServiceMode::Keyed);
        let id = service.id().to_string();
        let typed: LocalObserveHandler = Arc::new(move |target, guard, context| {
            let slot = ServiceSlot::new(&id);
            slot.bind(target);
            Box::pin(handler(slot.local_view::<T>(guard), context))
        });
        let (slots, service, check) = (self.slots.clone(), service.erase(), self.access());
        let start: ObservationStart = Box::new(move || {
            let stop = slots.observe_local(&service, check, typed)?;
            Ok(disposal_of(stop))
        });
        self.lifecycle().observe(start)
    }

    /// Declare and install this facet's singleton implementation of a remotely exposable service.
    pub fn provide(&self, service: &ServiceRef, implementation: ServiceImpl) -> ChordResult<()> {
        self.lifecycle().assert_setting_up("provide services")?;
        require_remote(service)?;
        self.push_singleton(service, SingletonValue::Remote(implementation));
        Ok(())
    }

    /// Declare and install an unrestricted process-local singleton.
    pub fn provide_local<T: ?Sized + Send + Sync + 'static>(
        &self,
        service: &Service<T>,
        implementation: Arc<T>,
    ) -> ChordResult<()> {
        self.lifecycle().assert_setting_up("provide services")?;
        require_local(service)?;
        self.push_singleton(service, SingletonValue::Local(local_impl(implementation)));
        Ok(())
    }

    fn push_singleton(&self, service: &ServiceRef, value: SingletonValue) {
        record(&self.runtime.provides, service, ServiceMode::Singleton);
        lock(&self.runtime.provisions).push(FacetProvision::Singleton { service: service.clone(), value });
    }

    fn push_keyed(&self, service: &ServiceRef) -> Arc<StagedServiceSpawner> {
        record(&self.runtime.provides, service, ServiceMode::Keyed);
        let spawner = StagedServiceSpawner::new(service, self.lifecycle().clone());
        let provision = FacetProvision::Keyed { service: service.clone(), spawner: spawner.clone() };
        lock(&self.runtime.provisions).push(provision);
        spawner
    }

    /// Declare ownership of a remotely exposable keyed service and return its deferred spawner.
    pub fn provide_many(&self, service: &ServiceRef) -> ChordResult<ServiceSpawner> {
        self.lifecycle().assert_setting_up("provide service instances")?;
        require_remote(service)?;
        Ok(ServiceSpawner(self.push_keyed(service)))
    }

    /// Declare ownership of a process-local keyed service and return its deferred spawner.
    pub fn provide_many_local<T: ?Sized + Send + Sync + 'static>(
        &self,
        service: &Service<T>,
    ) -> ChordResult<LocalServiceSpawner<T>> {
        self.lifecycle().assert_setting_up("provide service instances")?;
        require_local(service)?;
        Ok(LocalServiceSpawner { inner: self.push_keyed(service), marker: PhantomData })
    }

    /// Create mutable replicated state that owns `root`.
    pub fn replicated_state(&self, root: Json) -> ChordResult<MutableReplicatedState> {
        self.lifecycle().assert_running("create replicated state")?;
        Ok(replicated_state(root))
    }

    /// Give the facet ownership of a resource cleanup.
    pub fn own(&self, disposal: impl FnOnce() -> ChordResult<()> + Send + 'static) -> ChordResult<()> {
        self.lifecycle().own(Box::new(move || Box::pin(std::future::ready(disposal()))))
    }

    /// Give the facet ownership of an asynchronous resource cleanup.
    pub fn own_async<F, Fut>(&self, disposal: F) -> ChordResult<()>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ChordResult<()>> + Send + 'static,
    {
        self.lifecycle().own(Box::new(move || Box::pin(disposal())))
    }

    /// Register asynchronous initialization after dependencies are bound and ready.
    pub fn on_activate<F, Fut>(&self, callback: F) -> ChordResult<()>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ChordResult<()>> + Send + 'static,
    {
        self.lifecycle().on_activate(Box::new(move || Box::pin(callback())))
    }

    /// Register final facet teardown.
    pub fn on_deactivate<F, Fut>(&self, callback: F) -> ChordResult<()>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ChordResult<()>> + Send + 'static,
    {
        self.own_async(callback)
    }
}

fn disposal_of(stop: Unsubscribe) -> super::lifecycle::Disposal {
    Box::new(move || Box::pin(std::future::ready(stop.close())))
}
