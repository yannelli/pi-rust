//! Scripted provider for tests and offline demos. Register it with
//! `register_api_provider("faux", provider)` and use `FauxProvider::model()`.
//! Requests with `StreamOptions::deferred` return a deferred handle, as upstream `faux.ts` does.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::registry::{ApiProvider, DeferredOptions, ProviderResponse, StreamOptions};
use crate::stream::{AssistantMessageEventStream, MessageBuilder, error_stream, event_channel};
use crate::types::{ContentBlock, Context, DeferredHandle, Model, ModelCost, StopReason, Usage};

#[derive(Clone, Debug)]
pub enum FauxResponse {
    /// Stream these blocks, then finish with `stop`.
    Message { content: Vec<ContentBlock>, stop: StopReason },
    /// Fail with a provider error after `Start`.
    Error(String),
    /// Stream nothing until the request is cancelled.
    Hang,
}

impl FauxResponse {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Message { content: vec![ContentBlock::text(text)], stop: StopReason::Stop }
    }

    /// A response with one tool call; `arguments` must be a JSON object.
    pub fn tool_call(id: &str, name: &str, arguments: Value) -> Self {
        Self::tool_calls(&[(id, name, arguments)])
    }

    pub fn tool_calls(calls: &[(&str, &str, Value)]) -> Self {
        let content = calls
            .iter()
            .map(|(id, name, arguments)| {
                ContentBlock::ToolCall(crate::types::ToolCall {
                    id: (*id).into(),
                    name: (*name).into(),
                    arguments: arguments.as_object().cloned().unwrap_or_default(),
                    thought_signature: None,
                    ..Default::default()
                })
            })
            .collect();
        Self::Message { content, stop: StopReason::ToolUse }
    }
}

/// A submitted deferred request.
struct DeferredEntry {
    handle: DeferredHandle,
    response: FauxResponse,
    input_tokens: u64,
    pending_fetches: u32,
    cancelled: bool,
}

#[derive(Default)]
struct DeferredState {
    entries: HashMap<String, DeferredEntry>,
    next_id: u64,
    fetches: u32,
    cancelled: Vec<DeferredHandle>,
}

#[derive(Default)]
pub struct FauxProvider {
    responses: Mutex<VecDeque<FauxResponse>>,
    requests: Mutex<Vec<Context>>,
    chunk_delay: Duration,
    /// Fetches that return the handle again before the response is ready, and the poll hint.
    deferred_config: (u32, Option<u64>),
    deferred: Mutex<DeferredState>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl FauxProvider {
    pub fn new(responses: impl IntoIterator<Item = FauxResponse>) -> Arc<Self> {
        Arc::new(Self { responses: Mutex::new(responses.into_iter().collect()), ..Self::default() })
    }

    /// Wait `delay` between streamed chunks.
    pub fn with_delay(responses: impl IntoIterator<Item = FauxResponse>, delay: Duration) -> Arc<Self> {
        Arc::new(Self { responses: Mutex::new(responses.into_iter().collect()), chunk_delay: delay, ..Self::default() })
    }

    /// Deferred requests stay pending for `pending_fetches` fetches and suggest `poll_after_ms`.
    pub fn with_deferred(
        responses: impl IntoIterator<Item = FauxResponse>,
        pending_fetches: u32,
        poll_after_ms: Option<u64>,
    ) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into_iter().collect()),
            deferred_config: (pending_fetches, poll_after_ms),
            ..Self::default()
        })
    }

    pub fn push(&self, response: FauxResponse) {
        lock(&self.responses).push_back(response);
    }

    /// Contexts received so far, in order.
    pub fn requests(&self) -> Vec<Context> {
        lock(&self.requests).clone()
    }

    /// Number of `fetch_deferred` calls so far.
    pub fn deferred_fetch_count(&self) -> u32 {
        lock(&self.deferred).fetches
    }

    /// Handles passed to `cancel_deferred`, in order.
    pub fn cancelled_deferred(&self) -> Vec<DeferredHandle> {
        lock(&self.deferred).cancelled.clone()
    }

    pub fn model() -> Model {
        Model {
            id: "faux-model".into(),
            name: "Faux".into(),
            api: "faux".into(),
            provider: "faux".into(),
            base_url: String::new(),
            reasoning: true,
            input: vec!["text".into(), "image".into()],
            cost: ModelCost { input: 1.0, output: 2.0, cache_read: 0.0, cache_write: 0.0, ..Default::default() },
            context_window: 128_000,
            max_tokens: 8_192,
            headers: Default::default(),
            compat: None,
            ..Default::default()
        }
    }

    fn defer(&self, model: &Model, response: FauxResponse, input_tokens: u64) -> DeferredHandle {
        let mut state = lock(&self.deferred);
        state.next_id += 1;
        let handle = DeferredHandle {
            provider: model.provider.clone(),
            model_id: model.id.clone(),
            api: model.api.clone(),
            id: format!("deferred-{}", state.next_id),
            expires_at: None,
            poll_after_ms: self.deferred_config.1,
            data: None,
        };
        let entry = DeferredEntry {
            handle: handle.clone(),
            response,
            input_tokens,
            pending_fetches: self.deferred_config.0,
            cancelled: false,
        };
        state.entries.insert(handle.id.clone(), entry);
        handle
    }
}

fn respond_ok(model: &Model, on_response: Option<&crate::registry::OnResponseFn>) {
    if let Some(hook) = on_response {
        hook(&ProviderResponse { status: 200, headers: Default::default() }, model);
    }
}

impl ApiProvider for FauxProvider {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        lock(&self.requests).push(context.clone());
        let response = lock(&self.responses).pop_front();
        respond_ok(model, options.on_response.as_ref());
        let input_tokens = serde_json::to_string(context).map(|json| json.len() as u64 / 4).unwrap_or_default();
        let (sender, stream) = event_channel();
        let mut builder = MessageBuilder::new(model, sender);
        let Some(response) = response else {
            builder.fail(StopReason::Error, "No scripted faux response");
            return stream;
        };
        if options.deferred.is_some() {
            let handle = self.defer(model, response, input_tokens);
            builder.start();
            builder.defer(handle);
            return stream;
        }
        tokio::spawn(play(builder, response, self.chunk_delay, options.cancel, input_tokens, model.cost.clone()));
        stream
    }

    fn fetch_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredOptions,
    ) -> AssistantMessageEventStream {
        let mut state = lock(&self.deferred);
        state.fetches += 1;
        respond_ok(model, options.request.on_response.as_ref());
        let Some(entry) = state.entries.get_mut(&handle.id).filter(|entry| {
            (&entry.handle.provider, &entry.handle.model_id, &entry.handle.api)
                == (&handle.provider, &handle.model_id, &handle.api)
        }) else {
            return error_stream(model, StopReason::Error, format!("Unknown faux deferred response: {}", handle.id));
        };
        if entry.cancelled {
            let message = format!("Faux deferred response was cancelled: {}", handle.id);
            return error_stream(model, StopReason::Error, message);
        }
        let (sender, stream) = event_channel();
        let mut builder = MessageBuilder::new(model, sender);
        if entry.pending_fetches > 0 {
            entry.pending_fetches -= 1;
            builder.start();
            builder.defer(entry.handle.clone());
            return stream;
        }
        let (response, input_tokens) = (entry.response.clone(), entry.input_tokens);
        tokio::spawn(play(
            builder,
            response,
            self.chunk_delay,
            options.request.cancel,
            input_tokens,
            model.cost.clone(),
        ));
        stream
    }

    fn cancel_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredOptions,
    ) -> BoxFuture<'static, Result<(), String>> {
        let mut state = lock(&self.deferred);
        state.cancelled.push(handle.clone());
        if let Some(entry) = state.entries.get_mut(&handle.id) {
            entry.cancelled = true;
        }
        respond_ok(model, options.request.on_response.as_ref());
        Box::pin(async { Ok(()) })
    }
}

async fn play(
    mut builder: MessageBuilder,
    response: FauxResponse,
    delay: Duration,
    cancel: CancellationToken,
    input_tokens: u64,
    cost: ModelCost,
) {
    builder.start();
    let pause = || async {
        if !delay.is_zero() {
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = cancel.cancelled() => {}
            }
        }
    };
    let (content, stop) = match response {
        FauxResponse::Error(error) => return builder.fail(StopReason::Error, error),
        FauxResponse::Hang => {
            cancel.cancelled().await;
            return builder.fail(StopReason::Aborted, "Request aborted");
        }
        FauxResponse::Message { content, stop } => (content, stop),
    };
    let mut output_tokens = 0;
    for block in content {
        match block {
            ContentBlock::Text { text, .. } => {
                for chunk in chunks(&text) {
                    pause().await;
                    if cancel.is_cancelled() {
                        return builder.fail(StopReason::Aborted, "Request aborted");
                    }
                    output_tokens += 1;
                    builder.text_delta(&chunk);
                }
            }
            ContentBlock::Thinking { thinking, .. } => {
                for chunk in chunks(&thinking) {
                    pause().await;
                    output_tokens += 1;
                    builder.thinking_delta(&chunk);
                }
            }
            ContentBlock::ToolCall(call) => {
                pause().await;
                let handle = builder.tool_call_start(call.id, call.name);
                let json = Value::Object(call.arguments).to_string();
                for chunk in chunks(&json) {
                    output_tokens += 1;
                    builder.tool_call_delta(handle, &chunk);
                }
            }
            ContentBlock::Image { .. } => {}
        }
    }
    if cancel.is_cancelled() {
        return builder.fail(StopReason::Aborted, "Request aborted");
    }
    let mut usage = Usage {
        input: input_tokens,
        output: output_tokens,
        total_tokens: input_tokens + output_tokens,
        ..Usage::default()
    };
    usage.apply_cost(&cost);
    builder.message_mut().usage = usage;
    builder.finish(stop);
}

fn chunks(text: &str) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    characters.chunks(8).map(|chunk| chunk.iter().collect()).collect()
}

#[cfg(test)]
#[path = "faux_tests.rs"]
mod tests;
