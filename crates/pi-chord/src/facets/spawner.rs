//! Facet provisions and the staged spawners behind `provide_many`.

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use super::lifecycle::FacetLifecycle;
use super::slots::LocalKeyedServiceRegistry;
use crate::error::{ChordError, ChordResult};
use crate::services::handle::{LocalImpl, local_impl};
use crate::services::{RemoteServiceProvider, ServiceImpl, ServiceRef};
use crate::sync::{Unsubscribe, lock};

/// A keyed implementation as staged by a facet.
#[derive(Clone)]
pub(crate) enum StagedValue {
    Remote(ServiceImpl),
    Local(LocalImpl),
}

type Installer = Arc<dyn Fn(&str, StagedValue) -> ChordResult<Unsubscribe> + Send + Sync>;

struct StagedInstance {
    value: StagedValue,
    release: Mutex<Option<Unsubscribe>>,
}

/// Holds a facet's keyed instances until the host connects them to a provider or registry.
pub(crate) struct StagedServiceSpawner {
    service: ServiceRef,
    lifecycle: Arc<FacetLifecycle>,
    instances: Mutex<IndexMap<String, Arc<StagedInstance>>>,
    installer: Mutex<Option<Installer>>,
}

impl StagedServiceSpawner {
    pub fn new(service: &ServiceRef, lifecycle: Arc<FacetLifecycle>) -> Arc<Self> {
        Arc::new(Self {
            service: service.clone(),
            lifecycle,
            instances: Mutex::new(IndexMap::new()),
            installer: Mutex::new(None),
        })
    }

    pub fn connect(&self, installer: Installer) -> ChordResult<()> {
        {
            let mut current = lock(&self.installer);
            if current.is_some() {
                return Err(ChordError::error("Facet service provider is already connected"));
            }
            *current = Some(installer.clone());
        }
        let instances: Vec<(String, Arc<StagedInstance>)> =
            lock(&self.instances).iter().map(|(key, instance)| (key.clone(), instance.clone())).collect();
        for (key, instance) in instances {
            let release = installer(&key, instance.value.clone())?;
            *lock(&instance.release) = Some(release);
        }
        Ok(())
    }

    fn spawn(self: &Arc<Self>, key: &str, value: StagedValue) -> ChordResult<Unsubscribe> {
        self.lifecycle.assert_active("spawn service instances")?;
        if key.is_empty() {
            return Err(ChordError::type_error("Facet service instance key must not be empty"));
        }
        if let StagedValue::Remote(implementation) = &value {
            implementation.validate(self.service.id())?;
        }
        let instance = Arc::new(StagedInstance { value: value.clone(), release: Mutex::new(None) });
        {
            let mut instances = lock(&self.instances);
            if instances.contains_key(key) {
                return Err(ChordError::error(format!("Facet service already has a live instance with key {key}")));
            }
            instances.insert(key.to_string(), instance.clone());
        }
        let installer = lock(&self.installer).clone();
        if let Some(installer) = installer {
            match installer(key, value) {
                Ok(release) => *lock(&instance.release) = Some(release),
                Err(error) => {
                    lock(&self.instances).shift_remove(key);
                    return Err(error);
                }
            }
        }
        let (spawner, key) = (Arc::downgrade(self), key.to_string());
        let close = Unsubscribe::fallible(move || {
            let Some(spawner) = spawner.upgrade() else { return Ok(()) };
            {
                let mut instances = lock(&spawner.instances);
                match instances.get(&key) {
                    Some(current) if Arc::ptr_eq(current, &instance) => instances.shift_remove(&key),
                    _ => return Ok(()),
                };
            }
            let release = lock(&instance.release).take();
            release.map_or(Ok(()), |release| release.close())
        });
        let owned = close.clone();
        self.lifecycle.own(Box::new(move || Box::pin(async move { owned.close() })))?;
        Ok(close)
    }
}

/// Spawns instances of a remotely exposable keyed service while the facet is active.
#[derive(Clone)]
pub struct ServiceSpawner(pub(crate) Arc<StagedServiceSpawner>);

impl ServiceSpawner {
    /// Publish one instance; `close()` on the result withdraws it. The facet owns the instance.
    pub fn spawn(&self, key: &str, implementation: ServiceImpl) -> ChordResult<Unsubscribe> {
        self.0.spawn(key, StagedValue::Remote(implementation))
    }
}

/// Spawns instances of a process-local keyed service while the facet is active.
pub struct LocalServiceSpawner<T: ?Sized> {
    pub(crate) inner: Arc<StagedServiceSpawner>,
    pub(crate) marker: PhantomData<fn(Arc<T>)>,
}

impl<T: ?Sized> Clone for LocalServiceSpawner<T> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone(), marker: PhantomData }
    }
}

impl<T: ?Sized + Send + Sync + 'static> LocalServiceSpawner<T> {
    pub fn spawn(&self, key: &str, implementation: Arc<T>) -> ChordResult<Unsubscribe> {
        self.inner.spawn(key, StagedValue::Local(local_impl(implementation)))
    }
}

/// A singleton implementation staged by `provide` or `provide_local`.
#[derive(Clone)]
pub(crate) enum SingletonValue {
    Remote(ServiceImpl),
    Local(LocalImpl),
}

/// What a facet contributes to the host when it is assembled or reloaded.
#[derive(Clone)]
pub(crate) enum FacetProvision {
    Singleton { service: ServiceRef, value: SingletonValue },
    Keyed { service: ServiceRef, spawner: Arc<StagedServiceSpawner> },
}

impl FacetProvision {
    pub fn service(&self) -> &ServiceRef {
        match self {
            Self::Singleton { service, .. } | Self::Keyed { service, .. } => service,
        }
    }

    pub fn is_keyed(&self) -> bool {
        matches!(self, Self::Keyed { .. })
    }

    pub fn connect_local(&self, registry: &Arc<LocalKeyedServiceRegistry>) -> ChordResult<()> {
        let Self::Keyed { service, spawner } = self else { return Ok(()) };
        let (registry, service) = (registry.clone(), service.clone());
        spawner.connect(Arc::new(move |key, value| match value {
            StagedValue::Local(implementation) => registry.spawn(&service, key, implementation),
            StagedValue::Remote(_) => Err(ChordError::type_error("Local keyed services take local values")),
        }))
    }

    pub fn connect_remote(&self, provider: &RemoteServiceProvider) -> ChordResult<()> {
        let Self::Keyed { service, spawner } = self else { return Ok(()) };
        let (provider, service) = (provider.clone(), service.clone());
        spawner.connect(Arc::new(move |key, value| match value {
            StagedValue::Remote(implementation) => provider.spawn(&service, key, implementation),
            StagedValue::Local(_) => Err(ChordError::type_error("Remote keyed services take service implementations")),
        }))
    }
}
