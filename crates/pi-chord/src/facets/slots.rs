//! Host-owned service slots and the registry of process-local keyed services.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::context::Context;
use crate::error::{ChordError, ChordResult};
use crate::services::handle::{AccessCheck, LocalHandle, LocalImpl, ServiceHandle, ServiceSlot, SlotTarget};
use crate::services::instances::{InstanceDirectory, InstanceEntry, InstanceHandler, InstanceService};
use crate::services::{ObserveHandler, RemoteServices, ServiceRef};
use crate::state::ErrorReporter;
use crate::sync::{BoxFuture, Unsubscribe, lock};

struct LocalKeyedRegistration {
    generations: Mutex<HashMap<String, u64>>,
    directory: Arc<InstanceDirectory>,
}

/// Process-local keyed services: instances hold unrestricted Rust values.
pub(crate) struct LocalKeyedServiceRegistry {
    registrations: Mutex<HashMap<String, Arc<LocalKeyedRegistration>>>,
    disposed: AtomicBool,
}

impl LocalKeyedServiceRegistry {
    pub fn new(services: &[ServiceRef], report_error: ErrorReporter) -> ChordResult<Arc<Self>> {
        let mut registrations = HashMap::new();
        for service in services {
            let registration = Arc::new(LocalKeyedRegistration {
                generations: Mutex::new(HashMap::new()),
                directory: InstanceDirectory::new(true, report_error.clone()),
            });
            if registrations.insert(service.id().to_string(), registration).is_some() {
                return Err(ChordError::type_error("Local keyed service registry has duplicate IDs"));
            }
        }
        Ok(Arc::new(Self { registrations: Mutex::new(registrations), disposed: AtomicBool::new(false) }))
    }

    fn registration(&self, service_id: &str) -> ChordResult<Arc<LocalKeyedRegistration>> {
        if self.disposed.load(Ordering::SeqCst) {
            return Err(ChordError::error("Local keyed service registry is disposed"));
        }
        lock(&self.registrations)
            .get(service_id)
            .cloned()
            .ok_or_else(|| ChordError::error(format!("Local keyed service {service_id} is not registered")))
    }

    pub fn spawn(&self, service: &ServiceRef, key: &str, implementation: LocalImpl) -> ChordResult<Unsubscribe> {
        let registration = self.registration(service.id())?;
        if key.is_empty() {
            return Err(ChordError::type_error("Local service instance key must not be empty"));
        }
        if registration.directory.get(key).is_some() {
            let id = service.id();
            return Err(ChordError::error(format!("Local service {id} already has a live instance with key {key}")));
        }
        let generation = {
            let mut generations = lock(&registration.generations);
            let generation = generations.get(key).copied().unwrap_or(0) + 1;
            generations.insert(key.to_string(), generation);
            generation
        };
        let instance = Arc::new(InstanceEntry {
            key: key.to_string(),
            generation,
            service: InstanceService::Local(implementation),
            facade: None,
            deactivate: Box::new(|| {}),
        });
        registration.directory.insert(instance.clone())?;
        let directory = registration.directory.clone();
        Ok(Unsubscribe::new(move || directory.remove(&instance)))
    }

    pub fn observe(&self, service: &ServiceRef, handler: InstanceHandler) -> ChordResult<Unsubscribe> {
        self.registration(service.id())?.directory.observe(handler)
    }

    pub fn dispose(&self) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        let registrations: Vec<_> = lock(&self.registrations).drain().map(|(_, registration)| registration).collect();
        for registration in registrations {
            registration.directory.dispose();
        }
    }
}

/// Where a keyed service's instances come from.
#[derive(Clone)]
pub(crate) enum KeyedSource {
    Remote(Arc<dyn RemoteServices>),
    Local(Arc<LocalKeyedServiceRegistry>),
}

/// A typed handler for local keyed instances, erased by the environment.
pub(crate) type LocalObserveHandler =
    Arc<dyn Fn(SlotTarget, AccessCheck, Context) -> BoxFuture<ChordResult<()>> + Send + Sync>;

/// Stable slots that facet views read through, rebound across reloads.
#[derive(Default)]
pub(crate) struct HostServiceSlots {
    singletons: Mutex<HashMap<String, Arc<ServiceSlot>>>,
    keyed_sources: Mutex<HashMap<String, KeyedSource>>,
}

impl HostServiceSlots {
    fn singleton_slot(&self, service: &ServiceRef) -> Arc<ServiceSlot> {
        lock(&self.singletons).entry(service.id().to_string()).or_insert_with(|| ServiceSlot::new(service.id())).clone()
    }

    pub fn remote_singleton(&self, service: &ServiceRef, check: AccessCheck) -> ServiceHandle {
        self.singleton_slot(service).remote_view(check)
    }

    pub fn local_singleton<T: ?Sized + Send + Sync + 'static>(
        &self,
        service: &ServiceRef,
        check: AccessCheck,
    ) -> LocalHandle<T> {
        self.singleton_slot(service).local_view(check)
    }

    pub fn has_singleton(&self, service_id: &str) -> bool {
        lock(&self.singletons).contains_key(service_id)
    }

    pub fn bind_singleton(&self, service_id: &str, target: SlotTarget) {
        let slot = lock(&self.singletons).get(service_id).cloned();
        if let Some(slot) = slot {
            slot.bind(target);
        }
    }

    pub fn bind_keyed(&self, service_id: &str, source: KeyedSource) {
        lock(&self.keyed_sources).insert(service_id.to_string(), source);
    }

    fn closed_check(
        service: &ServiceRef,
        check: AccessCheck,
        stopped: Arc<AtomicBool>,
        context: Context,
    ) -> AccessCheck {
        let id = service.id().to_string();
        Arc::new(move || {
            check()?;
            if stopped.load(Ordering::SeqCst) || context.is_cancelled() {
                return Err(ChordError::error(format!("Keyed service {id} observation is closed")));
            }
            Ok(())
        })
    }

    fn source(&self, service: &ServiceRef) -> ChordResult<KeyedSource> {
        let source = lock(&self.keyed_sources).get(service.id()).cloned();
        source.ok_or_else(|| ChordError::error(format!("Service {} is disconnected", service.id())))
    }

    /// Watch a remotely exposable keyed service through guarded per-instance views.
    pub fn observe_remote(
        &self,
        service: &ServiceRef,
        check: AccessCheck,
        handler: ObserveHandler,
    ) -> ChordResult<Unsubscribe> {
        let KeyedSource::Remote(source) = self.source(service)? else {
            return Err(ChordError::type_error(format!("Service {} is process-local", service.id())));
        };
        let stopped = Arc::new(AtomicBool::new(false));
        let (flag, token) = (stopped.clone(), service.clone());
        let wrapped: ObserveHandler = Arc::new(move |target: ServiceHandle, context: Context| {
            let slot = ServiceSlot::new(token.id());
            slot.bind(SlotTarget::Remote(target));
            let view = slot.remote_view(Self::closed_check(&token, check.clone(), flag.clone(), context.clone()));
            handler(view, context)
        });
        let stop = source.observe(service, wrapped)?;
        Ok(Unsubscribe::new(move || {
            if !stopped.swap(true, Ordering::SeqCst) {
                stop.unsubscribe();
            }
        }))
    }

    /// Watch a process-local keyed service; the handler builds its typed view from the target.
    pub fn observe_local(
        &self,
        service: &ServiceRef,
        check: AccessCheck,
        handler: LocalObserveHandler,
    ) -> ChordResult<Unsubscribe> {
        let KeyedSource::Local(registry) = self.source(service)? else {
            return Err(ChordError::type_error(format!("Service {} is not process-local", service.id())));
        };
        let stopped = Arc::new(AtomicBool::new(false));
        let (flag, token) = (stopped.clone(), service.clone());
        let wrapped: InstanceHandler = Arc::new(move |instance, context: Context| {
            let target = match instance {
                InstanceService::Local(implementation) => SlotTarget::Local(implementation),
                InstanceService::Remote(handle) => SlotTarget::Remote(handle),
            };
            let guard = Self::closed_check(&token, check.clone(), flag.clone(), context.clone());
            handler(target, guard, context)
        });
        let stop = registry.observe(service, wrapped)?;
        Ok(Unsubscribe::new(move || {
            if !stopped.swap(true, Ordering::SeqCst) {
                stop.unsubscribe();
            }
        }))
    }

    pub fn dispose(&self) {
        let slots: Vec<_> = lock(&self.singletons).drain().map(|(_, slot)| slot).collect();
        for slot in slots {
            slot.unbind();
        }
        lock(&self.keyed_sources).clear();
    }
}
