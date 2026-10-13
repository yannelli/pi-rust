//! The consumer-side contract shared by service bindings and remote service sources.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;

use super::handle::{AccessCheck, ServiceHandle};
use super::token::ServiceRef;
use super::wire::ServiceCatalogueEntry;
use crate::context::Context;
use crate::error::ChordResult;
use crate::state::ErrorReporter;
use crate::sync::{BoxFuture, Unsubscribe};

/// A task started for each live keyed instance; its context aborts when the instance goes away.
pub type ObserveHandler = Arc<dyn Fn(ServiceHandle, Context) -> BoxFuture<ChordResult<()>> + Send + Sync>;

/// Build an [`ObserveHandler`] from an async closure.
pub fn observe_handler<F, Fut>(handler: F) -> ObserveHandler
where
    F: Fn(ServiceHandle, Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ChordResult<()>> + Send + 'static,
{
    Arc::new(move |service, context| Box::pin(handler(service, context)))
}

/// Services reached from outside a facet host, or bound through a transport.
#[async_trait]
pub trait RemoteServices: Send + Sync {
    /// The stable facade of a singleton service.
    fn use_service(&self, service: &ServiceRef) -> ChordResult<ServiceHandle>;
    /// Start a task per live instance of a keyed service.
    fn observe(&self, service: &ServiceRef, handler: ObserveHandler) -> ChordResult<Unsubscribe>;
    /// Wait until every service acquired so far has installed its first snapshot.
    async fn ready(&self, context: &Context) -> ChordResult<()>;
    async fn dispose(&self, context: &Context) -> ChordResult<()>;
}

/// What a facet host passes to a source when it opens bindings for the services it needs.
#[derive(Clone)]
pub struct RemoteServiceSourceOpenOptions {
    pub services: Vec<ServiceRef>,
    pub assert_access: AccessCheck,
    pub on_error: ErrorReporter,
}

/// Advertises services outside a facet host and opens bindings to them.
#[async_trait]
pub trait RemoteServiceSource: Send + Sync {
    /// Whether this source, while unavailable, may own services no catalogue lists for now.
    fn accepts_unavailable_services(&self) -> bool;
    async fn catalogue(&self, context: &Context) -> ChordResult<Vec<ServiceCatalogueEntry>>;
    fn open(&self, options: RemoteServiceSourceOpenOptions) -> ChordResult<Arc<dyn RemoteServices>>;
}
