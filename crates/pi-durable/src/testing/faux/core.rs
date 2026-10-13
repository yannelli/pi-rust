//! The faux provider itself (upstream `createFauxCore`): the response queue, deferred responses, and the
//! [`Models`] operations.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use futures::future::BoxFuture;
use pi_ai::registry::ProviderResponse;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Context, DeferredHandle, EventSender, Model,
    StopReason, Usage, event_channel, now_ms,
};

use super::stream::{Pacing, stream_with_deltas};
use super::usage::{PromptCache, with_usage_estimate};
use super::{FauxDeferredOptions, FauxProviderState, FauxResponseStep};
use crate::errors::DurableResult;
use crate::models::{DeferredCancelOptions, DeferredFetchOptions, Models, SimpleStreamOptions};
use crate::util::random_bytes;

/// `<prefix>:<milliseconds>:<random base 36>`, as upstream `randomId`.
pub(super) fn random_id(prefix: &str) -> String {
    let mut bytes = [0u8; 8];
    random_bytes(&mut bytes);
    let mut value = u64::from_le_bytes(bytes);
    let mut digits = Vec::new();
    while digits.len() < 11 {
        digits.push(char::from_digit((value % 36) as u32, 36).unwrap_or('0'));
        value /= 36;
    }
    format!("{prefix}:{}:{}", now_ms(), digits.into_iter().collect::<String>())
}

fn error_message(error: impl Into<String>, api: &str, provider: &str, model_id: &str) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: api.into(),
        provider: provider.into(),
        model: model_id.into(),
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some(error.into()),
        timestamp: now_ms(),
        ..AssistantMessage::default()
    }
}

fn deferred_message(model: &Model, handle: &DeferredHandle) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Usage::default(),
        stop_reason: StopReason::Deferred,
        deferred: Some(handle.clone()),
        timestamp: now_ms(),
        ..AssistantMessage::default()
    }
}

fn push_error(sender: &EventSender, message: AssistantMessage) {
    sender.push(AssistantMessageEvent::Error { reason: StopReason::Error, error: message });
}

struct DeferredEntry {
    handle: DeferredHandle,
    step: FauxResponseStep,
    context: Context,
    options: SimpleStreamOptions,
    model: Model,
    pending_fetches: u32,
    cancelled: bool,
    final_message: Option<AssistantMessage>,
}

#[derive(Default)]
struct Inner {
    pending: VecDeque<FauxResponseStep>,
    state: FauxProviderState,
    prompt_cache: PromptCache,
    deferred: HashMap<String, DeferredEntry>,
}

/// A scripted provider; see [`super::faux_provider`]. Clones share the queue and state.
#[derive(Clone)]
pub struct FauxProvider {
    api: String,
    provider: String,
    models: Vec<Model>,
    pacing: Pacing,
    deferred: Option<FauxDeferredOptions>,
    inner: Arc<Mutex<Inner>>,
}

impl FauxProvider {
    pub(super) fn new(
        api: String,
        provider: String,
        models: Vec<Model>,
        pacing: Pacing,
        deferred: Option<FauxDeferredOptions>,
    ) -> Self {
        Self { api, provider, models, pacing, deferred, inner: Arc::default() }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn provider_id(&self) -> &str {
        &self.provider
    }

    pub fn state(&self) -> FauxProviderState {
        self.lock().state.clone()
    }

    pub(super) fn set_responses(&self, responses: Vec<FauxResponseStep>) {
        self.lock().pending = responses.into();
    }

    pub(super) fn append_responses(&self, responses: Vec<FauxResponseStep>) {
        self.lock().pending.extend(responses);
    }

    pub(super) fn pending_response_count(&self) -> usize {
        self.lock().pending.len()
    }

    fn estimate(
        &self,
        message: AssistantMessage,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessage {
        let mut inner = self.lock();
        let session = options.session_id.as_deref();
        with_usage_estimate(message, &context.messages, session, options.cache_retention, &mut inner.prompt_cache)
    }

    /// Upstream `resolveResponse`: run a factory, then take this provider's ids and a usage estimate.
    async fn resolve_response(
        &self,
        step: &FauxResponseStep,
        context: &Context,
        options: &SimpleStreamOptions,
        model: &Model,
    ) -> Result<AssistantMessage, String> {
        let resolved = match step {
            FauxResponseStep::Message(message) => (**message).clone(),
            FauxResponseStep::Factory(factory) => {
                let state = self.state();
                factory(context, options, &state, model).await?
            }
        };
        let message = AssistantMessage {
            api: self.api.clone(),
            provider: self.provider.clone(),
            model: model.id.clone(),
            ..resolved
        };
        Ok(self.estimate(message, context, options))
    }

    async fn run_stream(
        self,
        sender: EventSender,
        step: Option<FauxResponseStep>,
        model: Model,
        context: Context,
        options: SimpleStreamOptions,
    ) {
        if let Some(on_response) = &options.on_response {
            on_response(&ProviderResponse { status: 200, ..ProviderResponse::default() }, &model);
        }
        let fail = |error: String| error_message(error, &self.api, &self.provider, &model.id);
        let Some(step) = step else {
            let message = self.estimate(fail("No more faux responses queued".into()), &context, &options);
            return push_error(&sender, message);
        };
        let signal = options.signal.clone();
        if options.deferred.as_ref().is_some_and(|deferred| deferred.is_requested()) {
            let handle = DeferredHandle {
                provider: model.provider.clone(),
                model_id: model.id.clone(),
                api: model.api.clone(),
                id: random_id("deferred"),
                expires_at: None,
                poll_after_ms: self.deferred.and_then(|deferred| deferred.poll_after_ms),
                data: None,
            };
            let entry = DeferredEntry {
                handle: handle.clone(),
                step,
                context,
                options,
                model: model.clone(),
                pending_fetches: self.deferred.map_or(0, |deferred| deferred.pending_fetches),
                cancelled: false,
                final_message: None,
            };
            self.lock().deferred.insert(handle.id.clone(), entry);
            let message = deferred_message(&model, &handle);
            if let Err(error) = stream_with_deltas(&sender, &message, self.pacing, signal.as_ref()).await {
                push_error(&sender, fail(error));
            }
            return;
        }
        let result = match self.resolve_response(&step, &context, &options, &model).await {
            Ok(message) => stream_with_deltas(&sender, &message, self.pacing, signal.as_ref()).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            push_error(&sender, fail(error));
        }
    }

    /// The message a deferred fetch streams, or the error it fails with.
    async fn deferred_result(&self, model: &Model, handle: &DeferredHandle) -> Result<AssistantMessage, String> {
        let (step, context, options, entry_model) = {
            let mut inner = self.lock();
            let entry = inner.deferred.get_mut(&handle.id).filter(|entry| {
                entry.handle.provider == handle.provider
                    && entry.handle.model_id == handle.model_id
                    && entry.handle.api == handle.api
            });
            let Some(entry) = entry else { return Err(format!("Unknown faux deferred response: {}", handle.id)) };
            if entry.cancelled {
                return Err(format!("Faux deferred response was cancelled: {}", handle.id));
            }
            if entry.pending_fetches > 0 {
                entry.pending_fetches -= 1;
                return Ok(deferred_message(model, &entry.handle));
            }
            if let Some(final_message) = &entry.final_message {
                return Ok(final_message.clone());
            }
            let submission =
                SimpleStreamOptions { deferred: None, signal: None, on_response: None, ..entry.options.clone() };
            (entry.step.clone(), entry.context.clone(), submission, entry.model.clone())
        };
        let final_message = match self.resolve_response(&step, &context, &options, &entry_model).await {
            Ok(message) => message,
            Err(error) => error_message(error, &self.api, &self.provider, &entry_model.id),
        };
        if let Some(entry) = self.lock().deferred.get_mut(&handle.id) {
            entry.final_message = Some(final_message.clone());
        }
        Ok(final_message)
    }
}

impl Models for FauxProvider {
    fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        match provider {
            Some(provider) if provider != self.provider => Vec::new(),
            _ => self.models.clone(),
        }
    }

    fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.models.iter().find(|model| model.provider == provider && model.id == id).cloned()
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let transcript = Context { messages: pi_ai::transcript::normalize_context(context), ..Context::default() };
        let step = {
            let mut inner = self.lock();
            inner.state.call_count += 1;
            inner.pending.pop_front()
        };
        let (sender, stream) = event_channel();
        let this = self.clone();
        tokio::spawn(this.run_stream(sender, step, model.clone(), transcript, options));
        stream
    }

    fn stream_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredFetchOptions,
    ) -> AssistantMessageEventStream {
        self.lock().state.deferred_fetch_count += 1;
        let (sender, stream) = event_channel();
        let this = self.clone();
        let (model, handle) = (model.clone(), handle.clone());
        tokio::spawn(async move {
            if let Some(on_response) = &options.on_response {
                on_response(&ProviderResponse { status: 200, ..ProviderResponse::default() }, &model);
            }
            let result = match this.deferred_result(&model, &handle).await {
                Ok(message) => stream_with_deltas(&sender, &message, this.pacing, options.signal.as_ref()).await,
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                push_error(&sender, error_message(error, &this.api, &this.provider, &model.id));
            }
        });
        stream
    }

    fn cancel_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredCancelOptions,
    ) -> BoxFuture<'static, DurableResult<()>> {
        {
            let mut inner = self.lock();
            inner.state.cancelled_deferred.push(handle.clone());
            if let Some(entry) = inner.deferred.get_mut(&handle.id) {
                entry.cancelled = true;
            }
        }
        if let Some(on_response) = &options.on_response {
            on_response(&ProviderResponse { status: 200, ..ProviderResponse::default() }, model);
        }
        Box::pin(async { Ok(()) })
    }
}
