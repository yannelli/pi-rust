//! Port of upstream `declare.ts`: what a factory declares through [`Declare`](super::object::Declare)
//! besides state cells, the shapes of resources and event sources.

use std::future::Future;
use std::sync::Arc;

use super::object::{Ref, ResourceHandle, ResourceImpl};
use crate::context::AbortSignal;
use crate::error::ChordResult;
use crate::json::Json;
use crate::sync::{BoxFuture, Unsubscribe};

/// Something that delivers events to subscribers.
pub trait Subscribable<E>: Send + Sync {
    fn subscribe(&self, listener: Box<dyn Fn(&E) + Send + Sync>) -> Unsubscribe;
}

/// An event source a listener can follow (`Declare::on`).
pub type EventSource = Arc<dyn Subscribable<Json>>;

/// What `open` receives.
#[derive(Clone)]
pub struct OpenOptions {
    pub signal: AbortSignal,
}

pub type Open<T> = Arc<dyn Fn(OpenOptions) -> BoxFuture<ChordResult<T>> + Send + Sync>;
pub type Close<T> = Arc<dyn Fn(T) -> BoxFuture<ChordResult<()>> + Send + Sync>;

/// A dependency of a resource: a reference or another resource of the object, compared by identity.
#[derive(Clone)]
pub enum Dependency {
    Ref(Ref),
    Resource(Arc<ResourceImpl>),
}

impl PartialEq for Dependency {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ref(left), Self::Ref(right)) => left.ptr_eq(right),
            (Self::Resource(left), Self::Resource(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
}

impl From<&Ref> for Dependency {
    fn from(reference: &Ref) -> Self {
        Self::Ref(reference.clone())
    }
}

impl<T> From<&ResourceHandle<T>> for Dependency {
    fn from(resource: &ResourceHandle<T>) -> Self {
        Self::Resource(resource.inner().clone())
    }
}

/// `self.resource(name, declaration)`: something the object opens and later closes (a database, a watch, a socket).
pub struct ResourceDeclaration<T> {
    /// Bump to replace the resource at the next reload (a native value has no schema to detect a change).
    pub version: i64,
    /// References or resources that must be live or open first. A change replaces the resource.
    pub deps: Vec<Dependency>,
    /// Exclusive resources are replaced at the gate: old closed before new opened.
    pub exclusive: bool,
    pub open: Open<T>,
    pub close: Option<Close<T>>,
}

impl<T> Clone for ResourceDeclaration<T> {
    fn clone(&self) -> Self {
        Self {
            version: self.version,
            deps: self.deps.clone(),
            exclusive: self.exclusive,
            open: self.open.clone(),
            close: self.close.clone(),
        }
    }
}

impl<T: Send + 'static> ResourceDeclaration<T> {
    /// A declaration of `version` that opens with `open`; it has no dependencies and no `close`.
    pub fn new<F, Fut>(version: i64, open: F) -> Self
    where
        F: Fn(OpenOptions) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChordResult<T>> + Send + 'static,
    {
        let open: Open<T> = Arc::new(move |options| Box::pin(open(options)));
        Self { version, deps: Vec::new(), exclusive: false, open, close: None }
    }

    /// A declaration whose `open` returns `value` at once.
    pub fn value(version: i64, value: T) -> Self
    where
        T: Clone + Sync,
    {
        Self::new(version, move |_| std::future::ready(Ok(value.clone())))
    }

    pub fn with_close<F, Fut>(mut self, close: F) -> Self
    where
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChordResult<()>> + Send + 'static,
    {
        self.close = Some(Arc::new(move |value| Box::pin(close(value))));
        self
    }

    pub fn with_deps(mut self, deps: Vec<Dependency>) -> Self {
        self.deps = deps;
        self
    }

    pub fn exclusive(mut self) -> Self {
        self.exclusive = true;
        self
    }
}

/// An opened resource as the factory sees it.
pub trait Resource<T>: Send + Sync {
    fn key(&self) -> &str;
    /// Fails with `not_ready` until opened, and while any factory of its host runs on this thread.
    fn value(&self) -> ChordResult<T>;
}
