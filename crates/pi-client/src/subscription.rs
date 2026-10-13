//! Service subscriptions and the Chord transport adapter over routed client targets.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::services::{
    RemoteServiceTransport, ServiceCall, ServiceMode, ServiceSubscription as ChordServiceSubscription,
    ServiceSubscriptionSnapshot, ServiceUpdateListener, create_service_state_decoder, create_service_subscribe_call,
    create_service_unsubscribe_call,
};
use pi_chord::{AbortSignal, BoxFuture, ChordError, ChordResult, Context, Json};
use pi_protocol::RpcTarget;

use crate::client::Client;
use crate::errors::ClientError;
use crate::state::{ActiveInner, ActiveService, Reply, lock};

/// One hydrated service subscription. Updates wait until [`ServiceSubscription::start`].
pub struct ServiceSubscription {
    pub id: String,
    pub target: RpcTarget,
    pub snapshot: ServiceSubscriptionSnapshot,
    client: Client,
    active: Arc<ActiveService>,
    disposed: AtomicBool,
}

impl ServiceSubscription {
    /// Begin ordered update delivery after the caller has installed the snapshot.
    pub fn start(&self) {
        if self.disposed.load(Ordering::SeqCst) {
            return;
        }
        {
            let mut inner = lock(&self.active.inner);
            if inner.ready {
                return;
            }
            inner.ready = true;
            let queued = std::mem::take(&mut inner.queued);
            inner.delivery.extend(queued);
        }
        self.client.inner.deliver(&self.active);
    }

    /// Stop delivery and unsubscribe when the route is still live. Idempotent.
    pub async fn dispose(&self) -> Result<(), ClientError> {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        {
            let mut state = lock(&self.client.inner.state);
            if state.services.get(&self.id).is_some_and(|current| Arc::ptr_eq(current, &self.active)) {
                state.services.remove(&self.id);
            }
        }
        let result = if self.client.connected() && self.client.target_is_current(&self.target) {
            self.client.request(self.target.clone(), create_service_unsubscribe_call(&self.id), None).await.map(drop)
        } else {
            Ok(())
        };
        let mut inner = lock(&self.active.inner);
        inner.queued_wire.clear();
        inner.queued.clear();
        inner.delivery.clear();
        result
    }
}

impl Client {
    /// Subscribe to one service. Resolves with the complete provider snapshot.
    pub async fn subscribe_service(
        &self,
        target: RpcTarget,
        service_id: &str,
        mode: ServiceMode,
        listener: ServiceUpdateListener,
        signal: Option<AbortSignal>,
    ) -> Result<ServiceSubscription, ClientError> {
        let active = Arc::new(ActiveService {
            listener,
            inner: Mutex::new(ActiveInner { decoder: Some(create_service_state_decoder()), ..ActiveInner::default() }),
        });
        let id = {
            let mut state = lock(&self.inner.state);
            state.subscription_sequence += 1;
            let id = format!("service-{}", state.subscription_sequence);
            state.services.insert(id.clone(), active.clone());
            id
        };
        let call = create_service_subscribe_call(&id, service_id, mode);
        let reply = self.inner.request(target.clone(), call, signal, Some(active.clone())).await;
        let still_active = {
            let mut state = lock(&self.inner.state);
            let current = state.services.get(&id).is_some_and(|current| Arc::ptr_eq(current, &active));
            if current && reply.is_err() {
                state.services.remove(&id);
            }
            current
        };
        let snapshot = match reply? {
            Reply::Snapshot(snapshot) => snapshot,
            Reply::Value(_) => return Err(ClientError::validation("Service subscription did not return a snapshot")),
        };
        if !still_active {
            return Err(ClientError::disconnected());
        }
        Ok(ServiceSubscription { id, target, snapshot, client: self.clone(), active, disposed: AtomicBool::new(false) })
    }
}

/// Resolves the routed target for each operation, or `None` while it is unavailable.
pub type TargetResolver = Arc<dyn Fn() -> Option<RpcTarget> + Send + Sync>;

struct ClientServiceTransport {
    client: Client,
    target: TargetResolver,
}

impl ClientServiceTransport {
    fn target(&self) -> ChordResult<RpcTarget> {
        (self.target)().ok_or_else(|| ChordError::error("Remote service target is unavailable"))
    }
}

struct SubscriptionAdapter(Arc<ServiceSubscription>);

impl ChordServiceSubscription for SubscriptionAdapter {
    fn snapshot(&self) -> &ServiceSubscriptionSnapshot {
        &self.0.snapshot
    }

    fn activate(&self) -> ChordResult<()> {
        self.0.start();
        Ok(())
    }

    fn close(&self, _context: &Context) -> BoxFuture<ChordResult<()>> {
        let subscription = self.0.clone();
        Box::pin(async move { subscription.dispose().await.map_err(ChordError::from) })
    }
}

#[async_trait]
impl RemoteServiceTransport for ClientServiceTransport {
    async fn invoke(&self, call: ServiceCall, context: &Context) -> ChordResult<Option<Json>> {
        Ok(self.client.request(self.target()?, call, context.abort_signal()).await?)
    }

    async fn subscribe(
        &self,
        service_id: &str,
        mode: ServiceMode,
        listener: ServiceUpdateListener,
        context: &Context,
    ) -> ChordResult<Arc<dyn ChordServiceSubscription>> {
        let target = self.target()?;
        let subscription =
            self.client.subscribe_service(target, service_id, mode, listener, context.abort_signal()).await?;
        Ok(Arc::new(SubscriptionAdapter(Arc::new(subscription))))
    }
}

/// Adapt a lazily resolved routed client target to a Chord service transport.
pub fn create_client_service_transport(client: &Client, target: TargetResolver) -> Arc<dyn RemoteServiceTransport> {
    Arc::new(ClientServiceTransport { client: client.clone(), target })
}
