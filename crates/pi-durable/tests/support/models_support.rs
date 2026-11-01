//! A `Models` wrapper standing in for upstream's `Proxy` over `models`: it replaces `stream_simple`,
//! `fetch_deferred`, or `cancel_deferred` and delegates the rest.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, DeferredHandle, Model, event_channel,
};
use pi_durable::models::{DeferredCancelOptions, DeferredFetchOptions, Models, SimpleStreamOptions};
use pi_durable::{DurableError, DurableResult};

pub type StreamFn = Arc<dyn Fn(&Model) -> AssistantMessageEventStream + Send + Sync>;
/// Replaces `fetch_deferred`; it gets the wrapped `Models` to delegate to.
pub type FetchFn = Arc<
    dyn Fn(&Arc<dyn Models>, &Model, &DeferredHandle, DeferredFetchOptions) -> BoxFuture<'static, AssistantMessage>
        + Send
        + Sync,
>;
pub type CancelFn = Arc<dyn Fn(DeferredCancelOptions) -> BoxFuture<'static, DurableResult<()>> + Send + Sync>;

pub struct WrappedModels {
    pub inner: Arc<dyn Models>,
    pub stream: Option<StreamFn>,
    /// When set, `cancel_deferred` rejects with this message.
    pub cancel_error: Option<String>,
    pub fetch: Option<FetchFn>,
    pub cancel: Option<CancelFn>,
}

impl WrappedModels {
    pub fn stream(inner: Arc<dyn Models>, stream: StreamFn) -> Arc<dyn Models> {
        Arc::new(Self { inner, stream: Some(stream), cancel_error: None, fetch: None, cancel: None })
    }

    pub fn failing_cancel(inner: Arc<dyn Models>, message: &str) -> Arc<dyn Models> {
        Arc::new(Self { inner, stream: None, cancel_error: Some(message.to_owned()), fetch: None, cancel: None })
    }

    /// `inner` with `fetch_deferred` replaced by `fetch`.
    pub fn fetching(inner: Arc<dyn Models>, fetch: FetchFn) -> Arc<dyn Models> {
        Arc::new(Self { inner, stream: None, cancel_error: None, fetch: Some(fetch), cancel: None })
    }

    /// `inner` with `cancel_deferred` replaced by `cancel`.
    pub fn cancelling(inner: Arc<dyn Models>, cancel: CancelFn) -> Arc<dyn Models> {
        Arc::new(Self { inner, stream: None, cancel_error: None, fetch: None, cancel: Some(cancel) })
    }
}

impl Models for WrappedModels {
    fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        self.inner.get_models(provider)
    }

    fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.inner.get_model(provider, id)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &pi_ai::Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        match &self.stream {
            Some(stream) => stream(model),
            None => self.inner.stream_simple(model, context, options),
        }
    }

    fn stream_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredFetchOptions,
    ) -> AssistantMessageEventStream {
        self.inner.stream_deferred(model, handle, options)
    }

    fn fetch_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredFetchOptions,
    ) -> BoxFuture<'static, AssistantMessage> {
        match &self.fetch {
            Some(fetch) => fetch(&self.inner, model, handle, options),
            None => self.inner.fetch_deferred(model, handle, options),
        }
    }

    fn cancel_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredCancelOptions,
    ) -> BoxFuture<'static, DurableResult<()>> {
        if let Some(cancel) = &self.cancel {
            return cancel(options);
        }
        match &self.cancel_error {
            Some(message) => {
                let error = DurableError::error(message.clone());
                Box::pin(async move { Err(error) })
            }
            None => self.inner.cancel_deferred(model, handle, options),
        }
    }
}

/// A stream that starts with `partial`, waits `wait_ms`, then ends with `last`.
pub fn scripted_stream(partial: AssistantMessage, wait_ms: u64, last: AssistantMessage) -> AssistantMessageEventStream {
    let (sender, stream) = event_channel();
    tokio::spawn(async move {
        sender.push(AssistantMessageEvent::Start { partial });
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        sender.push(AssistantMessageEvent::Done { reason: last.stop_reason, message: last });
    });
    stream
}
