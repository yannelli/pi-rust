//! Stable consumer handles: remote service facades, state facades, local handles, and host slots.

use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use crate::context::Context;
use crate::error::{ChordError, ChordResult};
use crate::json::Json;
use crate::state::StateListener;
use crate::sync::{BoxFuture, Unsubscribe, lock};

/// Fails when a handle is used outside the lifetime that owns it.
pub type AccessCheck = Arc<dyn Fn() -> ChordResult<()> + Send + Sync>;

pub(crate) fn always_allowed() -> AccessCheck {
    Arc::new(|| Ok(()))
}

pub(crate) trait ServiceTarget: Send + Sync {
    fn call(&self, member: &str, args: Vec<Json>, context: &Context) -> BoxFuture<ChordResult<Option<Json>>>;
    fn state(&self, member: &str) -> ChordResult<StateHandle>;
}

pub(crate) trait StateTarget: Send + Sync {
    fn value(&self) -> ChordResult<Option<Json>>;
    fn subscribe(&self, listener: StateListener) -> ChordResult<Unsubscribe>;
}

/// A stable handle to a remotely exposable service, local or remote. It survives provider
/// replacement; calls fail while the service is disconnected.
#[derive(Clone)]
pub struct ServiceHandle(pub(crate) Arc<dyn ServiceTarget>);

impl ServiceHandle {
    /// Invoke a method with strict-JSON business arguments. `None` is a void result.
    pub fn call(
        &self,
        member: &str,
        args: Vec<Json>,
        context: &Context,
    ) -> impl Future<Output = ChordResult<Option<Json>>> + Send + 'static {
        self.0.call(member, args, context)
    }

    /// The facade of a replicated state member.
    pub fn state(&self, member: &str) -> ChordResult<StateHandle> {
        self.0.state(member)
    }

    /// Whether both handles are the same stable facade.
    pub fn ptr_eq(&self, other: &ServiceHandle) -> bool {
        std::ptr::addr_eq(Arc::as_ptr(&self.0), Arc::as_ptr(&other.0))
    }
}

impl std::fmt::Debug for ServiceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ServiceHandle")
    }
}

/// A stable read-only facade of a replicated state member: `None` until hydrated.
#[derive(Clone)]
pub struct StateHandle(pub(crate) Arc<dyn StateTarget>);

impl StateHandle {
    pub fn value(&self) -> ChordResult<Option<Json>> {
        self.0.value()
    }

    pub fn subscribe(&self, listener: StateListener) -> ChordResult<Unsubscribe> {
        self.0.subscribe(listener)
    }
}

/// A local implementation stored type-erased; it holds an `Arc<T>`.
pub(crate) type LocalImpl = Arc<dyn Any + Send + Sync>;

pub(crate) fn local_impl<T: ?Sized + Send + Sync + 'static>(implementation: Arc<T>) -> LocalImpl {
    Arc::new(implementation)
}

pub(crate) fn downcast_local<T: ?Sized + Send + Sync + 'static>(implementation: &LocalImpl) -> ChordResult<Arc<T>> {
    implementation
        .downcast_ref::<Arc<T>>()
        .cloned()
        .ok_or_else(|| ChordError::type_error("Local service implementation has a different type"))
}

/// A stable handle to a process-local service; `get` resolves the current provider.
pub struct LocalHandle<T: ?Sized> {
    resolve: Arc<dyn Fn() -> ChordResult<LocalImpl> + Send + Sync>,
    marker: PhantomData<fn() -> Arc<T>>,
}

impl<T: ?Sized> Clone for LocalHandle<T> {
    fn clone(&self) -> Self {
        Self { resolve: self.resolve.clone(), marker: PhantomData }
    }
}

impl<T: ?Sized + Send + Sync + 'static> LocalHandle<T> {
    pub(crate) fn new(resolve: Arc<dyn Fn() -> ChordResult<LocalImpl> + Send + Sync>) -> Self {
        Self { resolve, marker: PhantomData }
    }

    /// The current implementation. Fails while disconnected or outside the owner's lifetime.
    pub fn get(&self) -> ChordResult<Arc<T>> {
        let implementation = (self.resolve)()?;
        downcast_local(&implementation)
    }
}

/// What a host slot points at.
#[derive(Clone)]
pub(crate) enum SlotTarget {
    Remote(ServiceHandle),
    Local(LocalImpl),
}

/// A host-owned mutable target behind consumer-owned guarded views.
pub(crate) struct ServiceSlot {
    service_id: Arc<str>,
    target: Mutex<Option<SlotTarget>>,
}

impl ServiceSlot {
    pub fn new(service_id: &str) -> Arc<Self> {
        Arc::new(Self { service_id: service_id.into(), target: Mutex::new(None) })
    }

    pub fn bind(&self, target: SlotTarget) {
        *lock(&self.target) = Some(target);
    }

    pub fn unbind(&self) {
        *lock(&self.target) = None;
    }

    fn resolve(&self, check: &AccessCheck) -> ChordResult<SlotTarget> {
        check()?;
        lock(&self.target)
            .clone()
            .ok_or_else(|| ChordError::error(format!("Service {} is disconnected", self.service_id)))
    }

    pub fn remote_view(self: &Arc<Self>, check: AccessCheck) -> ServiceHandle {
        ServiceHandle(Arc::new(SlotView { slot: self.clone(), check }))
    }

    pub fn local_view<T: ?Sized + Send + Sync + 'static>(self: &Arc<Self>, check: AccessCheck) -> LocalHandle<T> {
        let slot = self.clone();
        LocalHandle::new(Arc::new(move || match slot.resolve(&check)? {
            SlotTarget::Local(implementation) => Ok(implementation),
            SlotTarget::Remote(_) => Err(ChordError::type_error("Service is not process-local")),
        }))
    }
}

struct SlotView {
    slot: Arc<ServiceSlot>,
    check: AccessCheck,
}

impl SlotView {
    fn remote(&self) -> ChordResult<ServiceHandle> {
        match self.slot.resolve(&self.check)? {
            SlotTarget::Remote(handle) => Ok(handle),
            SlotTarget::Local(_) => Err(ChordError::type_error("Service member is not callable")),
        }
    }
}

impl ServiceTarget for SlotView {
    fn call(&self, member: &str, args: Vec<Json>, context: &Context) -> BoxFuture<ChordResult<Option<Json>>> {
        match self.remote() {
            Ok(handle) => handle.0.call(member, args, context),
            Err(error) => Box::pin(std::future::ready(Err(error))),
        }
    }

    fn state(&self, member: &str) -> ChordResult<StateHandle> {
        self.remote()?;
        Ok(StateHandle(Arc::new(SlotStateView {
            view: SlotView { slot: self.slot.clone(), check: self.check.clone() },
            member: member.to_string(),
        })))
    }
}

struct SlotStateView {
    view: SlotView,
    member: String,
}

impl StateTarget for SlotStateView {
    fn value(&self) -> ChordResult<Option<Json>> {
        self.view.remote()?.state(&self.member)?.value()
    }

    fn subscribe(&self, listener: StateListener) -> ChordResult<Unsubscribe> {
        self.view.remote()?.state(&self.member)?.subscribe(listener)
    }
}
