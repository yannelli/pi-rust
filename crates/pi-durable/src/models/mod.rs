//! Model access for the harness: the subset of upstream pi-ai `Models` that pi-durable calls (`getModel`,
//! `streamSimple`, `completeSimple`, `streamDeferred`, `fetchDeferred`, `cancelDeferred`), as an object-safe trait.
//! [`RegistryModels`] implements it over `pi_ai::ModelRegistry` and the global API provider registry.

mod registry;

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use pi_ai::registry::{OnPayloadFn, OnResponseFn};
use pi_ai::{
    AssistantMessage, AssistantMessageEventStream, CacheRetention, Context, DeferredHandle, JsonObject, Model,
    ThinkingBudgets, ThinkingLevel, Transport,
};
use pi_chord::AbortSignal;
use pi_telemetry::TelemetrySpan;
use serde::{Deserialize, Serialize};

use crate::errors::DurableResult;

pub use registry::{RegistryModels, create_models};

/// Upstream `SimpleStreamOptions["deferred"]`: `true`, or `{ window?: "15m" | "1h" | "24h" }`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DeferredOption {
    Enabled(bool),
    Window {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<String>,
    },
}

impl DeferredOption {
    /// Whether a deferred handle is requested (upstream tests the option for truthiness).
    pub fn is_requested(&self) -> bool {
        !matches!(self, Self::Enabled(false))
    }

    /// The `pi_ai::StreamOptions::deferred` value: the window, or an empty string for the provider default.
    pub fn to_stream_option(&self) -> Option<String> {
        match self {
            Self::Enabled(false) => None,
            Self::Enabled(true) | Self::Window { window: None } => Some(String::new()),
            Self::Window { window: Some(window) } => Some(window.clone()),
        }
    }
}

/// Upstream `SimpleStreamOptions` as `Models.streamSimple` takes it. `signal` cancels the request.
#[derive(Clone, Default)]
pub struct SimpleStreamOptions {
    pub signal: Option<AbortSignal>,
    /// Parent of the request's telemetry, such as the calling phase's span.
    pub telemetry: Option<Arc<dyn TelemetrySpan>>,
    pub api_key: Option<String>,
    /// Provider-scoped environment values over the process environment.
    pub env: BTreeMap<String, String>,
    /// Request headers; they win over resolved auth headers.
    pub headers: BTreeMap<String, String>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    pub sampling_params: Option<JsonObject>,
    pub max_tokens: Option<u64>,
    pub transport: Option<Transport>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub websocket_connect_timeout_ms: Option<u64>,
    pub metadata: Option<JsonObject>,
    /// `auto` or `none`; adapters choose when unset.
    pub tool_choice: Option<String>,
    pub reasoning: Option<ThinkingLevel>,
    pub deferred: Option<DeferredOption>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub on_payload: Option<OnPayloadFn>,
    pub on_response: Option<OnResponseFn>,
}

impl std::fmt::Debug for SimpleStreamOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SimpleStreamOptions")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("session_id", &self.session_id)
            .field("reasoning", &self.reasoning)
            .field("cache_retention", &self.cache_retention)
            .field("deferred", &self.deferred)
            .finish_non_exhaustive()
    }
}

/// Upstream `DeferredFetchOptions`; `DeferredCancelOptions` is the same struct without `wait_ms`.
#[derive(Clone, Default)]
pub struct DeferredFetchOptions {
    pub signal: Option<AbortSignal>,
    /// Parent of the request's telemetry, such as the calling phase's span.
    pub telemetry: Option<Arc<dyn TelemetrySpan>>,
    pub api_key: Option<String>,
    pub env: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    /// Longest provider long-poll in milliseconds; `None` or 0 checks once. Cancellation ignores it.
    pub wait_ms: Option<u64>,
    pub on_response: Option<OnResponseFn>,
}

pub type DeferredCancelOptions = DeferredFetchOptions;

impl std::fmt::Debug for DeferredFetchOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeferredFetchOptions")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("wait_ms", &self.wait_ms)
            .finish_non_exhaustive()
    }
}

/// The model access pi-durable needs. Streams always end with one `Done` or `Error` event; request failures
/// (unknown provider, missing auth) surface as `Error` events, as upstream `lazyStream` reports them.
pub trait Models: Send + Sync {
    /// Chat models of `provider`, or of every provider.
    fn get_models(&self, provider: Option<&str>) -> Vec<Model>;

    /// The chat model `provider`/`id`.
    fn get_model(&self, provider: &str, id: &str) -> Option<Model>;

    /// Stream a response. `context.system_prompt` and `context.tools` fold into a leading system message.
    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream;

    /// Stream a response and return the final message.
    fn complete_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> BoxFuture<'static, AssistantMessage> {
        let stream = self.stream_simple(model, context, options);
        let model = model.clone();
        Box::pin(async move { stream.result(&model).await })
    }

    /// Stream the continuation of a deferred response; a still pending one ends with stop reason `Deferred`.
    fn stream_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredFetchOptions,
    ) -> AssistantMessageEventStream;

    /// Fetch a deferred response and return the final message.
    fn fetch_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredFetchOptions,
    ) -> BoxFuture<'static, AssistantMessage> {
        let stream = self.stream_deferred(model, handle, options);
        let model = model.clone();
        Box::pin(async move { stream.result(&model).await })
    }

    /// Best-effort cancellation of a deferred response. Fails like upstream rejects (unknown provider, no auth).
    fn cancel_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredCancelOptions,
    ) -> BoxFuture<'static, DurableResult<()>>;
}

/// A terminal `Error` stream for `model` carrying `message`, the way upstream `lazyStream` reports setup failures.
pub fn setup_error_stream(model: &Model, message: impl Into<String>) -> AssistantMessageEventStream {
    pi_ai::error_stream(model, pi_ai::StopReason::Error, message)
}
