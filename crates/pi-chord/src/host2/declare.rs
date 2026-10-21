//! Port of upstream `declare.ts`: what a factory declares through `self` besides state cells, the shapes of
//! resources and event sources. Listener declarations need the call context and come with the object layer.

use std::any::Any;
use std::sync::Arc;

use crate::context::AbortSignal;
use crate::error::ChordResult;
use crate::sync::{BoxFuture, Unsubscribe};

/// Something that delivers events to subscribers.
pub trait Subscribable<E>: Send + Sync {
    fn subscribe(&self, listener: Box<dyn Fn(&E) + Send + Sync>) -> Unsubscribe;
}

/// What `open` receives.
#[derive(Clone)]
pub struct OpenOptions {
    pub signal: AbortSignal,
}

pub type Open<T> = Arc<dyn Fn(OpenOptions) -> BoxFuture<ChordResult<T>> + Send + Sync>;
pub type Close<T> = Arc<dyn Fn(T) -> BoxFuture<ChordResult<()>> + Send + Sync>;

/// A dependency of a resource: a reference or another resource, compared by identity.
#[derive(Clone)]
pub struct Dependency(pub Arc<dyn Any + Send + Sync>);

impl PartialEq for Dependency {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
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

/// An opened resource as the factory sees it.
pub trait Resource<T>: Send + Sync {
    fn key(&self) -> &str;
    /// Fails with `not_ready` until opened.
    fn value(&self) -> ChordResult<T>;
}
