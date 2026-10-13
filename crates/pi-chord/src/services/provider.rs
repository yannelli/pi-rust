//! The provider side of remote services: singleton and keyed registrations, calls, and subscriptions.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use super::provider_subscriber::{ProviderSubscriber, ProviderSubscription, ServiceUpdateListener};
use super::token::{MemberKind, ServiceImpl, ServiceMember, ServiceRef};
use super::wire::{
    ServiceCall, ServiceCatalogueEntry, ServiceInstanceAddress, ServiceInstanceSnapshot, ServiceMemberSnapshotOf,
    ServiceMode, ServiceProviderUpdate, ServiceProviderUpdateOf, ServiceSubscriptionSnapshot,
};
use crate::context::Context;
use crate::delta::Op;
use crate::error::{ChordError, ChordResult, RemoteServiceErrorCode as Code};
use crate::json::Json;
use crate::state::service_delivery_context;
use crate::sync::{Unsubscribe, lock};

/// One service the provider may publish, with its mode.
#[derive(Clone, Debug)]
pub struct ServiceDefinition {
    pub service: ServiceRef,
    pub mode: ServiceMode,
}

impl ServiceDefinition {
    pub fn singleton(service: &ServiceRef) -> Self {
        Self { service: service.clone(), mode: ServiceMode::Singleton }
    }

    pub fn keyed(service: &ServiceRef) -> Self {
        Self { service: service.clone(), mode: ServiceMode::Keyed }
    }
}

pub(crate) struct Instance {
    address: Option<ServiceInstanceAddress>,
    implementation: ServiceImpl,
    active: Arc<AtomicBool>,
    listeners: Mutex<Vec<Unsubscribe>>,
}

impl Instance {
    fn deactivate(&self) {
        self.active.store(false, Ordering::SeqCst);
        for listener in lock(&self.listeners).drain(..) {
            listener.unsubscribe();
        }
    }

    fn snapshot(&self) -> ServiceInstanceSnapshot {
        let members = self
            .implementation
            .members()
            .map(|(name, member)| match member {
                ServiceMember::Method(_) => ServiceMemberSnapshotOf::Method { name: name.clone() },
                ServiceMember::State(state) => {
                    let (value, sequence) = state.snapshot();
                    ServiceMemberSnapshotOf::State { name: name.clone(), sequence, ops: vec![Op::Replace(value)] }
                }
            })
            .collect();
        ServiceInstanceSnapshot { instance: self.address.clone(), members }
    }
}

struct Registration {
    mode: ServiceMode,
    singleton: Option<Arc<Instance>>,
    singleton_shape: Option<BTreeMap<String, MemberKind>>,
    instances: BTreeMap<String, Arc<Instance>>,
    generations: HashMap<String, u64>,
    subscribers: Vec<Arc<ProviderSubscriber>>,
}

impl Registration {
    fn snapshot(&self, service_id: &str) -> ServiceSubscriptionSnapshot {
        let instances = match self.mode {
            ServiceMode::Singleton => self.singleton.iter().map(|instance| instance.snapshot()).collect(),
            ServiceMode::Keyed => self.instances.values().map(|instance| instance.snapshot()).collect(),
        };
        ServiceSubscriptionSnapshot { service_id: service_id.to_string(), mode: self.mode, instances }
    }
}

pub(crate) struct ProviderInner {
    catalogue: Vec<ServiceCatalogueEntry>,
    registrations: Mutex<HashMap<String, Registration>>,
    disposed: AtomicBool,
}

/// Hosts remotely exposable services and the subscriptions of their consumers.
#[derive(Clone)]
pub struct RemoteServiceProvider(pub(crate) Arc<ProviderInner>);

impl std::fmt::Debug for RemoteServiceProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteServiceProvider").field("catalogue", &self.0.catalogue).finish()
    }
}

fn not_allowed(service_id: &str) -> ChordError {
    ChordError::remote(Code::ServiceNotAllowed, format!("Remote service {service_id} is not allowlisted"))
}

fn assert_remotable(service: &ServiceRef) -> ChordResult<()> {
    if service.is_local() {
        return Err(ChordError::remote(Code::ServiceNotAllowed, format!("Service {} is process-local", service.id())));
    }
    Ok(())
}

impl ProviderInner {
    pub(crate) fn remove_subscriber(&self, service_id: &str, subscriber: &Arc<ProviderSubscriber>) {
        if let Some(registration) = lock(&self.registrations).get_mut(service_id) {
            registration.subscribers.retain(|other| !Arc::ptr_eq(other, subscriber));
        }
    }

    fn emit(&self, service_id: &str, update: ServiceProviderUpdate, context: Option<&Context>) -> ChordResult<()> {
        let context = context.cloned().unwrap_or_else(service_delivery_context);
        let subscribers = {
            let registrations = lock(&self.registrations);
            let Some(registration) = registrations.get(service_id) else { return Ok(()) };
            let reset = || registration.snapshot(service_id);
            // Queue for everyone before user code runs, including reentrant publications.
            for subscriber in &registration.subscribers {
                subscriber.enqueue(&update, &context, &reset);
            }
            registration.subscribers.clone()
        };
        let errors = subscribers.iter().flat_map(|subscriber| subscriber.drain()).collect();
        ChordError::collect(errors, &format!("Failed to publish remote service {service_id} update"))
    }
}

impl RemoteServiceProvider {
    /// Allowlist the services this provider may publish.
    pub fn new(definitions: &[ServiceDefinition]) -> ChordResult<Self> {
        for definition in definitions {
            if definition.service.is_local() {
                let id = definition.service.id();
                return Err(ChordError::type_error(format!("Local service {id} cannot be published remotely")));
            }
        }
        let mut registrations = HashMap::new();
        let mut catalogue = Vec::new();
        for definition in definitions {
            let id = definition.service.id().to_string();
            if registrations.contains_key(&id) {
                return Err(ChordError::type_error("Remote service catalogue contains duplicate IDs"));
            }
            catalogue.push(ServiceCatalogueEntry { service_id: id.clone(), mode: definition.mode });
            registrations.insert(
                id,
                Registration {
                    mode: definition.mode,
                    singleton: None,
                    singleton_shape: None,
                    instances: BTreeMap::new(),
                    generations: HashMap::new(),
                    subscribers: Vec::new(),
                },
            );
        }
        Ok(Self(Arc::new(ProviderInner {
            catalogue,
            registrations: Mutex::new(registrations),
            disposed: AtomicBool::new(false),
        })))
    }

    pub fn catalogue(&self) -> &[ServiceCatalogueEntry] {
        &self.0.catalogue
    }

    fn assert_active(&self) -> ChordResult<()> {
        if self.0.disposed.load(Ordering::SeqCst) {
            return Err(ChordError::error("Remote service provider is disposed"));
        }
        Ok(())
    }

    fn with_registration<R>(
        &self,
        service_id: &str,
        mode: Option<ServiceMode>,
        action: impl FnOnce(&mut Registration) -> ChordResult<R>,
    ) -> ChordResult<R> {
        self.assert_active()?;
        let mut registrations = lock(&self.0.registrations);
        let registration = registrations.get_mut(service_id).ok_or_else(|| not_allowed(service_id))?;
        if let Some(mode) = mode
            && registration.mode != mode
        {
            let message = format!("Remote service {service_id} is {}, not {mode}", registration.mode);
            return Err(ChordError::remote(Code::ServiceModeMismatch, message));
        }
        action(registration)
    }

    fn create_instance(
        &self,
        service_id: &str,
        implementation: ServiceImpl,
        address: Option<ServiceInstanceAddress>,
    ) -> Arc<Instance> {
        let active = Arc::new(AtomicBool::new(true));
        let mut listeners = Vec::new();
        for (name, member) in implementation.members() {
            let ServiceMember::State(state) = member else { continue };
            let (provider, active, address, name) =
                (Arc::downgrade(&self.0), active.clone(), address.clone(), name.clone());
            let service_id = service_id.to_string();
            listeners.push(state.subscribe_ops(move |ops, sequence, context| {
                let Some(provider) = provider.upgrade() else { return Ok(()) };
                if !active.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let update = ServiceProviderUpdateOf::State {
                    instance: address.clone(),
                    member: name.clone(),
                    sequence,
                    ops: ops.to_vec(),
                };
                provider.emit(&service_id, update, Some(context))
            }));
        }
        Arc::new(Instance { address, implementation, active, listeners: Mutex::new(listeners) })
    }

    fn assert_shape(registration: &Registration, service_id: &str, implementation: &ServiceImpl) -> ChordResult<()> {
        match &registration.singleton_shape {
            Some(shape) if *shape != implementation.shape() => Err(ChordError::remote(
                Code::ServiceMemberMismatch,
                format!("Remote service {service_id} replacement must preserve its member shape"),
            )),
            _ => Ok(()),
        }
    }

    /// Install the singleton implementation.
    pub fn provide(&self, service: &ServiceRef, implementation: ServiceImpl) -> ChordResult<()> {
        assert_remotable(service)?;
        let id = service.id();
        self.with_registration(id, Some(ServiceMode::Singleton), |registration| {
            if registration.singleton.is_some() {
                let message = format!("Remote service {id} already has a provider");
                return Err(ChordError::remote(Code::ServiceModeMismatch, message));
            }
            implementation.validate(id)?;
            Self::assert_shape(registration, id, &implementation)?;
            registration.singleton_shape = Some(implementation.shape());
            registration.singleton = Some(self.create_instance(id, implementation, None));
            Ok(())
        })
    }

    /// Disconnect one singleton while keeping subscriptions and remote facades.
    pub fn withdraw(&self, service: &ServiceRef) -> ChordResult<()> {
        assert_remotable(service)?;
        let id = service.id();
        let previous =
            self.with_registration(id, Some(ServiceMode::Singleton), |registration| Ok(registration.singleton.take()))?;
        let Some(previous) = previous else { return Ok(()) };
        previous.deactivate();
        self.0.emit(id, ServiceProviderUpdateOf::Unavailable, None)
    }

    /// Check a singleton replacement without changing the active provider.
    pub fn validate_replacement(&self, service: &ServiceRef, implementation: &ServiceImpl) -> ChordResult<()> {
        assert_remotable(service)?;
        let id = service.id();
        self.with_registration(id, Some(ServiceMode::Singleton), |registration| {
            implementation.validate(id)?;
            Self::assert_shape(registration, id, implementation)
        })
    }

    /// Replace one singleton without making its stable remote facade unavailable.
    pub fn replace(&self, service: &ServiceRef, implementation: ServiceImpl) -> ChordResult<()> {
        assert_remotable(service)?;
        let id = service.id();
        let snapshot = self.with_registration(id, Some(ServiceMode::Singleton), |registration| {
            implementation.validate(id)?;
            Self::assert_shape(registration, id, &implementation)?;
            let shape = implementation.shape();
            let replacement = self.create_instance(id, implementation, None);
            if let Some(previous) = registration.singleton.replace(replacement.clone()) {
                previous.deactivate();
            }
            registration.singleton_shape = Some(shape);
            Ok(replacement.snapshot())
        })?;
        self.0.emit(id, ServiceProviderUpdateOf::Replaced { snapshot }, None)
    }

    /// The local singleton implementation.
    pub fn use_service(&self, service: &ServiceRef) -> ChordResult<ServiceImpl> {
        assert_remotable(service)?;
        let id = service.id();
        self.with_registration(id, None, |registration| match (&registration.mode, &registration.singleton) {
            (ServiceMode::Singleton, Some(instance)) => Ok(instance.implementation.clone()),
            _ => Err(ChordError::remote(Code::ServiceNotFound, format!("Remote service {id} has no local provider"))),
        })
    }

    /// Publish a keyed instance; `close()` on the returned disposer closes it and returns publication failures.
    pub fn spawn(&self, service: &ServiceRef, key: &str, implementation: ServiceImpl) -> ChordResult<Unsubscribe> {
        assert_remotable(service)?;
        let id = service.id();
        if key.is_empty() {
            return Err(ChordError::type_error("Remote service instance key must not be empty"));
        }
        let (instance, snapshot) = self.with_registration(id, Some(ServiceMode::Keyed), |registration| {
            if registration.instances.contains_key(key) {
                let message = format!("Remote service {id} already has a live instance with key {key}");
                return Err(ChordError::remote(Code::ServiceModeMismatch, message));
            }
            implementation.validate(id)?;
            let generation = registration.generations.get(key).copied().unwrap_or(0) + 1;
            registration.generations.insert(key.to_string(), generation);
            let address = ServiceInstanceAddress::new(key, generation);
            let instance = self.create_instance(id, implementation, Some(address));
            registration.instances.insert(key.to_string(), instance.clone());
            Ok((instance.clone(), instance.snapshot()))
        })?;
        self.0.emit(id, ServiceProviderUpdateOf::Spawned { instance: snapshot }, None)?;
        let (provider, id, key) = (Arc::downgrade(&self.0), id.to_string(), key.to_string());
        Ok(Unsubscribe::fallible(move || close_instance(&provider, &id, &key, &instance)))
    }

    /// Invoke a method on the addressed singleton or keyed instance.
    pub async fn invoke(&self, call: ServiceCall, context: &Context) -> ChordResult<Option<Json>> {
        let method = self.with_registration(&call.service_id, None, |registration| {
            let instance = resolve_instance(registration, &call.service_id, &call.instance)?;
            let not_found = || {
                let message = format!("Unknown remote service member {}.{}", call.service_id, call.member);
                ChordError::remote(Code::ServiceMemberNotFound, message)
            };
            match instance.implementation.member(&call.member).ok_or_else(not_found)? {
                ServiceMember::Method(method) => Ok(method.clone()),
                ServiceMember::State(_) => Err(ChordError::remote(
                    Code::ServiceMemberMismatch,
                    format!("Remote service member {}.{} is not a method", call.service_id, call.member),
                )),
            }
        })?;
        method(call.args, context.clone()).await
    }

    /// Capture an atomic snapshot and buffer later updates until activation.
    pub fn subscribe(
        &self,
        service_id: &str,
        mode: ServiceMode,
        listener: ServiceUpdateListener,
    ) -> ChordResult<ProviderSubscription> {
        let (snapshot, subscriber) = self.with_registration(service_id, Some(mode), |registration| {
            if registration.mode == ServiceMode::Singleton && registration.singleton.is_none() {
                let message = format!("Remote service {service_id} has no provider");
                return Err(ChordError::remote(Code::ServiceNotFound, message));
            }
            let snapshot = registration.snapshot(service_id);
            let subscriber = ProviderSubscriber::new(listener, &snapshot);
            registration.subscribers.push(subscriber.clone());
            Ok((snapshot, subscriber))
        })?;
        Ok(ProviderSubscription {
            snapshot,
            subscriber,
            provider: Arc::downgrade(&self.0),
            service_id: service_id.to_string(),
        })
    }

    /// Withdraw every service, close every instance, and end every subscription.
    pub fn dispose(&self) -> ChordResult<()> {
        if self.0.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let mut errors = Vec::new();
        let ids: Vec<String> = self.0.catalogue.iter().map(|entry| entry.service_id.clone()).collect();
        for id in &ids {
            let singleton =
                lock(&self.0.registrations).get_mut(id).and_then(|registration| registration.singleton.take());
            if let Some(singleton) = singleton {
                singleton.deactivate();
                errors.extend(self.0.emit(id, ServiceProviderUpdateOf::Unavailable, None).err());
            }
            let instances = lock(&self.0.registrations)
                .get_mut(id)
                .map(|registration| std::mem::take(&mut registration.instances))
                .unwrap_or_default();
            for instance in instances.into_values() {
                instance.deactivate();
                let Some(address) = instance.address.clone() else { continue };
                errors.extend(self.0.emit(id, ServiceProviderUpdateOf::Closed { instance: address }, None).err());
            }
            let subscribers = lock(&self.0.registrations)
                .get_mut(id)
                .map(|registration| std::mem::take(&mut registration.subscribers))
                .unwrap_or_default();
            for subscriber in subscribers {
                subscriber.terminate();
            }
        }
        lock(&self.0.registrations).clear();
        ChordError::collect(errors, "Failed to dispose remote service provider")
    }
}

fn close_instance(
    provider: &Weak<ProviderInner>,
    service_id: &str,
    key: &str,
    instance: &Arc<Instance>,
) -> ChordResult<()> {
    let Some(provider) = provider.upgrade() else { return Ok(()) };
    let removed = {
        let mut registrations = lock(&provider.registrations);
        let Some(registration) = registrations.get_mut(service_id) else { return Ok(()) };
        match registration.instances.get(key) {
            Some(current) if Arc::ptr_eq(current, instance) => registration.instances.remove(key),
            _ => None,
        }
    };
    let Some(instance) = removed else { return Ok(()) };
    instance.deactivate();
    match instance.address.clone() {
        Some(address) => provider.emit(service_id, ServiceProviderUpdateOf::Closed { instance: address }, None),
        None => Ok(()),
    }
}

fn resolve_instance<'a>(
    registration: &'a Registration,
    service_id: &str,
    address: &Option<ServiceInstanceAddress>,
) -> ChordResult<&'a Arc<Instance>> {
    match (registration.mode, address) {
        (ServiceMode::Singleton, Some(_)) => {
            Err(ChordError::remote(Code::ServiceModeMismatch, format!("Remote service {service_id} is singleton")))
        }
        (ServiceMode::Singleton, None) => registration.singleton.as_ref().ok_or_else(|| {
            ChordError::remote(Code::ServiceNotFound, format!("Remote service {service_id} has no provider"))
        }),
        (ServiceMode::Keyed, None) => {
            Err(ChordError::remote(Code::ServiceModeMismatch, format!("Remote service {service_id} is keyed")))
        }
        (ServiceMode::Keyed, Some(address)) => {
            let instance = registration.instances.get(&address.key).ok_or_else(|| {
                let message = format!("Remote service {service_id} has no instance {}", address.key);
                ChordError::remote(Code::ServiceInstanceNotFound, message)
            })?;
            if instance.address.as_ref().map(|current| current.generation) != Some(address.generation) {
                let message = format!("Remote service {service_id} instance {} is stale", address.key);
                return Err(ChordError::remote(Code::ServiceStaleInstance, message));
            }
            Ok(instance)
        }
    }
}
