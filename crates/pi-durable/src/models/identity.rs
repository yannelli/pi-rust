//! Upstream `withClientIdentity`: a [`Models`] view that gives every request a default client identity.

use std::sync::Arc;

use futures::future::BoxFuture;
use pi_ai::{AssistantMessage, AssistantMessageEventStream, ClientIdentity, Context, DeferredHandle, Model};

use super::{DeferredCancelOptions, DeferredFetchOptions, Models, SimpleStreamOptions};
use crate::errors::DurableResult;

/// `models` with `identity` as the default identity of its requests. A request's `identity` overrides it field by
/// field, and it overrides the identity `models` was created with. Everything else is delegated unchanged.
pub fn with_client_identity(models: Arc<dyn Models>, identity: ClientIdentity) -> Arc<dyn Models> {
    Arc::new(IdentityModels { models, identity })
}

struct IdentityModels {
    models: Arc<dyn Models>,
    identity: ClientIdentity,
}

impl IdentityModels {
    fn merged(&self, identity: Option<ClientIdentity>) -> Option<ClientIdentity> {
        pi_ai::merge_client_identity([Some(&self.identity), identity.as_ref()])
    }

    fn simple(&self, mut options: SimpleStreamOptions) -> SimpleStreamOptions {
        options.identity = self.merged(options.identity.take());
        options
    }

    fn deferred(&self, mut options: DeferredFetchOptions) -> DeferredFetchOptions {
        options.identity = self.merged(options.identity.take());
        options
    }
}

impl Models for IdentityModels {
    fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        self.models.get_models(provider)
    }

    fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.models.get_model(provider, id)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.models.stream_simple(model, context, self.simple(options))
    }

    fn complete_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> BoxFuture<'static, AssistantMessage> {
        self.models.complete_simple(model, context, self.simple(options))
    }

    fn stream_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredFetchOptions,
    ) -> AssistantMessageEventStream {
        self.models.stream_deferred(model, handle, self.deferred(options))
    }

    fn fetch_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredFetchOptions,
    ) -> BoxFuture<'static, AssistantMessage> {
        self.models.fetch_deferred(model, handle, self.deferred(options))
    }

    fn cancel_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredCancelOptions,
    ) -> BoxFuture<'static, DurableResult<()>> {
        self.models.cancel_deferred(model, handle, self.deferred(options))
    }
}
