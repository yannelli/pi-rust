//! The pluggable remote boundary consumed by service bindings, and the in-process loopback.

use std::sync::Arc;

use async_trait::async_trait;

use super::provider::RemoteServiceProvider;
use super::provider_subscriber::ServiceUpdateListener;
use super::wire::{ServiceCall, ServiceMode, ServiceSubscriptionSnapshot};
use crate::context::Context;
use crate::error::ChordResult;
use crate::json::Json;
use crate::sync::BoxFuture;

/// One consumer subscription as a transport delivers it.
pub trait ServiceSubscription: Send + Sync {
    /// Atomic baseline. Later updates buffer until activation, with a full reset on pending update 101.
    fn snapshot(&self) -> &ServiceSubscriptionSnapshot;
    fn activate(&self) -> ChordResult<()>;
    fn close(&self, context: &Context) -> BoxFuture<ChordResult<()>>;
}

/// Carries logical calls and subscriptions. Implementations choose transport, framing, routing,
/// and envelope encoding; values crossing it are strict JSON.
#[async_trait]
pub trait RemoteServiceTransport: Send + Sync {
    async fn invoke(&self, call: ServiceCall, context: &Context) -> ChordResult<Option<Json>>;
    async fn subscribe(
        &self,
        service_id: &str,
        mode: ServiceMode,
        listener: ServiceUpdateListener,
        context: &Context,
    ) -> ChordResult<Arc<dyn ServiceSubscription>>;
}

/// Connects a provider to a binding without changing remote service semantics.
pub struct LoopbackServiceTransport {
    provider: RemoteServiceProvider,
}

/// Create a loopback transport over a provider in the same process.
pub fn create_loopback_service_transport(provider: &RemoteServiceProvider) -> Arc<dyn RemoteServiceTransport> {
    Arc::new(LoopbackServiceTransport { provider: provider.clone() })
}

#[async_trait]
impl RemoteServiceTransport for LoopbackServiceTransport {
    async fn invoke(&self, call: ServiceCall, context: &Context) -> ChordResult<Option<Json>> {
        self.provider.invoke(call, context).await
    }

    async fn subscribe(
        &self,
        service_id: &str,
        mode: ServiceMode,
        listener: ServiceUpdateListener,
        _context: &Context,
    ) -> ChordResult<Arc<dyn ServiceSubscription>> {
        Ok(Arc::new(self.provider.subscribe(service_id, mode, listener)?))
    }
}
