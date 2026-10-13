//! Service tokens and remote service implementations.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::Arc;

use crate::context::Context;
use crate::error::{ChordError, ChordResult};
use crate::json::Json;
use crate::state::StateMember;
use crate::sync::BoxFuture;

/// The type-erased identity of a service: its stable ID and locality.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ServiceRef {
    id: Arc<str>,
    local: bool,
}

impl ServiceRef {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Process-local services accept unrestricted Rust values and are never published remotely.
    pub fn is_local(&self) -> bool {
        self.local
    }

    /// A remote service reference by ID, for allowlists and catalogues.
    pub fn remote(id: impl Into<Arc<str>>) -> Self {
        Self { id: id.into(), local: false }
    }
}

impl fmt::Debug for ServiceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Service({}{})", self.id, if self.local { ", local" } else { "" })
    }
}

/// A stable service token. `T` names the contract: for a local service, the implementation type
/// consumers receive; for a remote service, a marker only.
pub struct Service<T: ?Sized> {
    inner: ServiceRef,
    marker: PhantomData<fn() -> Arc<T>>,
}

impl<T: ?Sized> Clone for Service<T> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone(), marker: PhantomData }
    }
}

impl<T: ?Sized> fmt::Debug for Service<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl<T: ?Sized> Deref for Service<T> {
    type Target = ServiceRef;

    fn deref(&self) -> &ServiceRef {
        &self.inner
    }
}

impl<T: ?Sized> Service<T> {
    /// The type-erased reference.
    pub fn erase(&self) -> ServiceRef {
        self.inner.clone()
    }
}

fn validate_id(id: &str) -> ChordResult<()> {
    if id.is_empty() {
        return Err(ChordError::type_error("Service ID must not be empty"));
    }
    if id.starts_with("$chord.") {
        return Err(ChordError::type_error("Service IDs beginning with $chord. are reserved"));
    }
    Ok(())
}

/// Define a remotely exposable service token.
pub fn define_service<T: ?Sized>(id: &str) -> ChordResult<Service<T>> {
    validate_id(id)?;
    Ok(Service { inner: ServiceRef { id: id.into(), local: false }, marker: PhantomData })
}

/// Define a process-local service token whose consumers receive `Arc<T>`.
pub fn define_local_service<T: ?Sized>(id: &str) -> ChordResult<Service<T>> {
    validate_id(id)?;
    Ok(Service { inner: ServiceRef { id: id.into(), local: true }, marker: PhantomData })
}

/// An asynchronous remote method: strict-JSON business arguments plus the invocation context.
pub type MethodFn = Arc<dyn Fn(Vec<Json>, Context) -> BoxFuture<ChordResult<Option<Json>>> + Send + Sync>;

/// One remotely exposable member.
#[derive(Clone)]
pub enum ServiceMember {
    Method(MethodFn),
    State(StateMember),
}

/// The kind of a member, as snapshots describe it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemberKind {
    Method,
    State,
}

impl MemberKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MemberKind::Method => "method",
            MemberKind::State => "state",
        }
    }
}

impl ServiceMember {
    pub fn kind(&self) -> MemberKind {
        match self {
            ServiceMember::Method(_) => MemberKind::Method,
            ServiceMember::State(_) => MemberKind::State,
        }
    }
}

/// A remote service implementation: named asynchronous methods and replicated states.
///
/// This is the Rust form of upstream's implementation object, whose members Chord classifies at runtime.
#[derive(Clone, Default)]
pub struct ServiceImpl {
    members: BTreeMap<String, ServiceMember>,
}

impl fmt::Debug for ServiceImpl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.members.iter().map(|(name, member)| (name, member.kind()))).finish()
    }
}

impl ServiceImpl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an asynchronous method. Its result is `None` for a void method.
    pub fn method<F, Fut>(mut self, name: impl Into<String>, method: F) -> Self
    where
        F: Fn(Vec<Json>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChordResult<Option<Json>>> + Send + 'static,
    {
        let method: MethodFn = Arc::new(move |args, context| Box::pin(method(args, context)));
        self.members.insert(name.into(), ServiceMember::Method(method));
        self
    }

    /// Add a replicated state member.
    pub fn state(mut self, name: impl Into<String>, state: impl Into<StateMember>) -> Self {
        self.members.insert(name.into(), ServiceMember::State(state.into()));
        self
    }

    /// Members sorted by name, as upstream classifies them.
    pub fn members(&self) -> impl Iterator<Item = (&String, &ServiceMember)> {
        self.members.iter()
    }

    pub fn member(&self, name: &str) -> Option<&ServiceMember> {
        self.members.get(name)
    }

    /// Invoke a method directly, as a caller holding the implementation object would.
    pub fn call(&self, name: &str, args: Vec<Json>, context: &Context) -> BoxFuture<ChordResult<Option<Json>>> {
        match self.members.get(name) {
            Some(ServiceMember::Method(method)) => method(args, context.clone()),
            _ => {
                let message = format!("Service member {name} is not a method");
                Box::pin(std::future::ready(Err(ChordError::type_error(message))))
            }
        }
    }

    pub(crate) fn validate(&self, service_id: &str) -> ChordResult<()> {
        if self.members.is_empty() {
            return Err(ChordError::type_error(format!("Remote service {service_id} has no members")));
        }
        Ok(())
    }

    pub(crate) fn shape(&self) -> BTreeMap<String, MemberKind> {
        self.members.iter().map(|(name, member)| (name.clone(), member.kind())).collect()
    }
}
