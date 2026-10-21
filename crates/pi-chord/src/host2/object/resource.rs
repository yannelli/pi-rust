//! Resources an object opens before it goes live and closes after it is disposed, and the members they back.

use std::any::Any;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use crate::context::AbortSignal;
use crate::error::{ChordError, ChordResult};
use crate::host2::cell::not_ready;
use crate::host2::declare::{Dependency, OpenOptions, Resource, ResourceDeclaration};
use crate::host2::errors::{ChordErrorCode, ErrorOptions};
use crate::host2::exec::in_factory;
use crate::sync::{BoxFuture, lock};

/// An opened resource's value, of the type its declaration names.
pub type AnyValue = Arc<dyn Any + Send + Sync>;

type OpenAny = Arc<dyn Fn(OpenOptions) -> BoxFuture<ChordResult<AnyValue>> + Send + Sync>;
type CloseAny = Arc<dyn Fn(AnyValue) -> BoxFuture<ChordResult<()>> + Send + Sync>;

/// A declaration with its value type erased.
#[derive(Clone)]
pub(crate) struct ErasedDeclaration {
    pub version: i64,
    pub exclusive: bool,
    open: OpenAny,
    close: Option<CloseAny>,
}

impl<T: Clone + Send + Sync + 'static> From<ResourceDeclaration<T>> for ErasedDeclaration {
    fn from(declaration: ResourceDeclaration<T>) -> Self {
        let open = declaration.open;
        let open: OpenAny = Arc::new(move |options| {
            let opening = open(options);
            Box::pin(async move { opening.await.map(|value| Arc::new(value) as AnyValue) })
        });
        let close = declaration.close.map(|close| {
            let close: CloseAny = Arc::new(move |value: AnyValue| match value.downcast_ref::<T>() {
                Some(value) => close(value.clone()),
                None => Box::pin(std::future::ready(Ok(()))),
            });
            close
        });
        Self { version: declaration.version, exclusive: declaration.exclusive, open, close }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Closed,
    Opening,
    Open,
    /// A close or reopen failed during a reload rollback: the value is gone for good.
    #[allow(dead_code, reason = "reload rollbacks (phase 4)")]
    Unavailable,
}

struct ResourceState {
    phase: Phase,
    value: Option<AnyValue>,
}

/// One declared resource of an object.
pub struct ResourceImpl {
    key: String,
    /// The declaration of the current code's factory run: a kept resource adopts the new one at commit.
    spec: Mutex<ErasedDeclaration>,
    pub(crate) deps: Vec<Dependency>,
    state: Mutex<ResourceState>,
}

impl ResourceImpl {
    pub(crate) fn new(key: &str, spec: ErasedDeclaration, deps: Vec<Dependency>) -> Arc<Self> {
        let state = ResourceState { phase: Phase::Closed, value: None };
        Arc::new(Self { key: key.to_string(), spec: Mutex::new(spec), deps, state: Mutex::new(state) })
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn version(&self) -> i64 {
        lock(&self.spec).version
    }

    pub(crate) fn exclusive(&self) -> bool {
        lock(&self.spec).exclusive
    }

    pub(crate) fn value(&self) -> ChordResult<AnyValue> {
        let state = lock(&self.state);
        match (state.phase, &state.value) {
            (Phase::Unavailable, _) => Err(ChordError::code_only(
                ChordErrorCode::Unavailable,
                format!("resource {} could not be restored", self.key),
            )),
            (Phase::Open, Some(value)) => Ok(value.clone()),
            _ => Err(not_ready(&format!("resource {}", self.key))),
        }
    }

    /// Opens the resource. An open that completes after `signal` aborted closes its value at once.
    pub(crate) async fn open(self: &Arc<Self>, signal: &AbortSignal, order: &Mutex<Vec<Arc<Self>>>) -> ChordResult<()> {
        let spec = {
            let mut state = lock(&self.state);
            if state.phase != Phase::Closed {
                return Ok(());
            }
            state.phase = Phase::Opening;
            lock(&self.spec).clone()
        };
        let value = match (spec.open)(OpenOptions { signal: signal.clone() }).await {
            Ok(value) => value,
            Err(error) => {
                lock(&self.state).phase = Phase::Closed;
                return Err(error);
            }
        };
        if signal.aborted() {
            lock(&self.state).phase = Phase::Closed;
            if let Some(close) = &spec.close {
                close(value).await?;
            }
            let message = format!("resource {} opened after abort", self.key);
            return Err(ChordError::coded(ChordErrorCode::Aborted, message, ErrorOptions::default()));
        }
        {
            let mut state = lock(&self.state);
            state.value = Some(value);
            state.phase = Phase::Open;
        }
        lock(order).push(self.clone());
        Ok(())
    }

    pub(crate) async fn close(self: &Arc<Self>, order: &Mutex<Vec<Arc<Self>>>) -> ChordResult<()> {
        let value = {
            let mut state = lock(&self.state);
            if state.phase != Phase::Open {
                return Ok(());
            }
            state.phase = Phase::Closed;
            state.value.take()
        };
        lock(order).retain(|resource| !Arc::ptr_eq(resource, self));
        let close = lock(&self.spec).close.clone();
        match (close, value) {
            (Some(close), Some(value)) => close(value).await,
            _ => Ok(()),
        }
    }

    #[allow(dead_code, reason = "reload rollbacks (phase 4)")]
    pub(crate) fn mark_unavailable(&self) {
        let mut state = lock(&self.state);
        state.phase = Phase::Unavailable;
        state.value = None;
    }

    /// A reload kept this resource: its later `close` is the new declaration's.
    #[allow(dead_code, reason = "reload commits (phase 4)")]
    pub(crate) fn adopt(&self, spec: ErasedDeclaration) {
        *lock(&self.spec) = spec;
    }
}

/// What [`Declare::resource`](super::Declare::resource) returns: the resource with its value type. Its value
/// fails `not_ready` while a factory of its host runs on this thread, even if open.
pub struct ResourceHandle<T> {
    resource: Arc<ResourceImpl>,
    host: u64,
    value: PhantomData<fn() -> T>,
}

impl<T> Clone for ResourceHandle<T> {
    fn clone(&self) -> Self {
        Self { resource: self.resource.clone(), host: self.host, value: PhantomData }
    }
}

impl<T> ResourceHandle<T> {
    pub(crate) fn new(resource: Arc<ResourceImpl>, host: u64) -> Self {
        Self { resource, host, value: PhantomData }
    }

    pub fn inner(&self) -> &Arc<ResourceImpl> {
        &self.resource
    }

    pub fn key(&self) -> &str {
        self.resource.key()
    }
}

impl<T: Clone + 'static> ResourceHandle<T> {
    pub fn value(&self) -> ChordResult<T> {
        if in_factory(self.host) {
            return Err(not_ready(&format!("resource {} (inside a factory)", self.resource.key())));
        }
        let value = self.resource.value()?;
        value
            .downcast_ref::<T>()
            .cloned()
            .ok_or_else(|| ChordError::type_error(format!("resource {} holds another type", self.resource.key())))
    }
}

impl<T: Clone + 'static> Resource<T> for ResourceHandle<T> {
    fn key(&self) -> &str {
        self.resource.key()
    }

    fn value(&self) -> ChordResult<T> {
        ResourceHandle::value(self)
    }
}

/// A member backed by a resource (`Declare::expose`): references read the resource's value once live.
pub(crate) struct Exposed {
    pub resource: Arc<ResourceImpl>,
}
