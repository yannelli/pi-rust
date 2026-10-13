//! Hosts one provider for one remote consumer and owns that consumer's subscriptions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::provider::RemoteServiceProvider;
use super::provider_subscriber::ProviderSubscription;
use super::wire::{ServiceCall, ServiceProviderUpdate, catalogue_to_json};
use super::wire_parse::{ServiceControlCall, decode_service_control_call};
use crate::context::Context;
use crate::error::{ChordError, ChordResult};
use crate::json::Json;
use crate::sync::lock;

/// Forwards one subscription update to the consumer. Failures stay with the adapter.
pub type ServiceUpdatePublisher = Arc<dyn Fn(&str, ServiceProviderUpdate, &Context) + Send + Sync>;

/// Handles `$chord.service` control calls and ordinary calls for one consumer.
pub struct RemoteServiceEndpoint {
    provider: RemoteServiceProvider,
    subscriptions: Mutex<HashMap<String, ProviderSubscription>>,
    disposed: AtomicBool,
}

/// Create an endpoint over `provider` for one remote consumer.
pub fn create_remote_service_endpoint(provider: &RemoteServiceProvider) -> RemoteServiceEndpoint {
    RemoteServiceEndpoint {
        provider: provider.clone(),
        subscriptions: Mutex::new(HashMap::new()),
        disposed: AtomicBool::new(false),
    }
}

impl RemoteServiceEndpoint {
    /// Answer one call. A subscribe returns the decoded snapshot; updates go through `publish`.
    pub async fn invoke(
        &self,
        call: ServiceCall,
        publish: ServiceUpdatePublisher,
        context: &Context,
    ) -> ChordResult<Option<Json>> {
        if self.disposed.load(Ordering::SeqCst) {
            return Err(ChordError::error("Remote service endpoint is disposed"));
        }
        match decode_service_control_call(&call) {
            Some(ServiceControlCall::Catalogue) => Ok(Some(catalogue_to_json(self.provider.catalogue()))),
            Some(ServiceControlCall::Subscribe { subscription_id, service_id, mode }) => {
                if lock(&self.subscriptions).contains_key(&subscription_id) {
                    return Err(ChordError::error("Service subscription ID is already active"));
                }
                let id = subscription_id.clone();
                let subscription = self.provider.subscribe(
                    &service_id,
                    mode,
                    Arc::new(move |update, context| {
                        publish(&id, update, context);
                        Ok(())
                    }),
                )?;
                let snapshot = subscription.snapshot().to_json();
                let subscriber = subscription.subscriber.clone();
                lock(&self.subscriptions).insert(subscription_id, subscription);
                // Activation runs listeners, so it happens outside the subscription table lock.
                subscriber.activate()?;
                Ok(Some(snapshot))
            }
            Some(ServiceControlCall::Unsubscribe { subscription_id }) => {
                let subscription = lock(&self.subscriptions).remove(&subscription_id);
                let subscription =
                    subscription.ok_or_else(|| ChordError::error("Service subscription was not found"))?;
                subscription.close();
                Ok(None)
            }
            None => self.provider.invoke(call, context).await,
        }
    }

    /// Close every subscription. Idempotent.
    pub fn dispose(&self) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        let subscriptions: Vec<ProviderSubscription> = lock(&self.subscriptions).drain().map(|(_, s)| s).collect();
        for subscription in subscriptions {
            subscription.close();
        }
    }
}
