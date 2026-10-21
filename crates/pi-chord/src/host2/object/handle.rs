//! [`Ref`], a claim on an object as code holds it, and [`RefCore`], what every kind of reference provides.

use std::ops::Deref;
use std::sync::Arc;

use super::resource::AnyValue;
use super::{Ctx, ObjectImpl};
use crate::error::{ChordError, ChordResult};
use crate::host2::contract::Contract;
use crate::host2::exec::{ErrorHandler, guarded};
use crate::host2::publish::enqueue;
use crate::host2::types::{ReadableRef, RefStatus, Shape, ShapeListener, StatusListener};
use crate::json::Json;
use crate::sync::{BoxFuture, Unsubscribe};

/// Who a reference belongs to. `Connection`: a peer host over a link; `Client`: a browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefOwner {
    Object,
    Call,
    Host,
    External,
    Connection,
    Client,
}

/// A claim on an object: a local [`RefImpl`](super::RefImpl), or (phase 3) a reference on another host.
pub trait RefCore: Send + Sync + 'static {
    fn address(&self) -> &str;
    fn contract(&self) -> &Contract;
    fn status(&self) -> RefStatus;
    /// The error that made the reference gone.
    fn error(&self) -> Option<ChordError>;
    /// Settles once the object is live, or fails with the reason it never will be.
    fn ready(&self) -> BoxFuture<ChordResult<()>>;
    /// The object, for a reference on this host.
    fn object(&self) -> Option<Arc<ObjectImpl>>;
    fn variant(&self) -> Option<String>;
    fn shape(&self) -> Option<Shape>;
    fn on_shape(&self, listener: ShapeListener) -> Unsubscribe;
    fn on_status(&self, listener: StatusListener) -> Unsubscribe;
    fn release(&self);
    /// Whether `name` is a member the reference reaches now (a declared method or state member always is).
    fn has(&self, name: &str) -> bool;
    /// The state member `name`: a stable readable across reloads and variants.
    fn state(&self, name: &str) -> Option<ReadableRef>;
    /// A native member's value (an exposed resource's opened value).
    fn opaque(&self, name: &str) -> ChordResult<Option<AnyValue>>;
    /// Calls method `name`. `None`: the method returns nothing (void); a method without arguments ignores
    /// `args`. Without a context, or with a task's, the call runs in a fresh root.
    fn call(&self, name: &str, args: Option<Json>, ctx: Option<&Ctx>) -> BoxFuture<ChordResult<Option<Json>>>;
}

/// A reference, as code holds it.
#[derive(Clone)]
pub struct Ref(Arc<dyn RefCore>);

impl Ref {
    pub fn new(core: Arc<dyn RefCore>) -> Self {
        Self(core)
    }

    pub fn core(&self) -> &Arc<dyn RefCore> {
        &self.0
    }

    pub fn ptr_eq(&self, other: &Ref) -> bool {
        std::ptr::addr_eq(Arc::as_ptr(&self.0), Arc::as_ptr(&other.0))
    }

    /// [`RefCore::call`] for a method that returns a value; a void result reads as `null`.
    pub async fn call_json(&self, name: &str, args: Json, ctx: Option<&Ctx>) -> ChordResult<Json> {
        Ok(self.0.call(name, Some(args), ctx).await?.unwrap_or_default())
    }

    /// The exposed or opaque member `name`, as type `T`.
    pub fn opaque_as<T: Clone + 'static>(&self, name: &str) -> ChordResult<Option<T>> {
        Ok(self.0.opaque(name)?.and_then(|value| value.downcast_ref::<T>().cloned()))
    }
}

impl Deref for Ref {
    type Target = dyn RefCore;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl std::fmt::Debug for Ref {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ref").field("address", &self.address()).field("status", &self.status()).finish()
    }
}

/// Queues a status notification (after the change that caused it is complete, like every publication).
pub(crate) fn notify_status(
    listeners: Vec<StatusListener>,
    status: RefStatus,
    error: Option<ChordError>,
    report: ErrorHandler,
) {
    if listeners.is_empty() {
        return;
    }
    enqueue(move || {
        for listener in listeners {
            guarded(&report, || listener(status, error.as_ref()));
        }
    });
}
