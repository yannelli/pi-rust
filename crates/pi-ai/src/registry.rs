//! Provider registry and the top-level `stream` and `complete` entry points.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, RwLock};

use futures::future::BoxFuture;
use pi_telemetry::TelemetrySpan;
use tokio_util::sync::CancellationToken;

use crate::env::get_env_api_key;
use crate::identity::ClientIdentity;
use crate::provider_options::{AzureOptions, BedrockOptions, ReasoningSummary, VertexOptions};
use crate::providers;
use crate::request::RequestOptions;
use crate::stream::{AssistantMessageEventStream, error_stream};
use crate::telemetry::observe_model_request;
use crate::types::{AssistantMessage, Context, DeferredHandle, Model, StopReason, ThinkingLevel};

/// Preferred transport for providers that support several.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    #[default]
    Sse,
    Websocket,
    WebsocketCached,
    Auto,
}

/// Prompt cache retention preference; providers map it to supported values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheRetention {
    None,
    #[default]
    Short,
    Long,
}

/// Token budgets per thinking level for token-based providers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThinkingBudgets {
    pub minimal: Option<u64>,
    pub low: Option<u64>,
    pub medium: Option<u64>,
    pub high: Option<u64>,
}

/// HTTP status and headers of a provider response, passed to `on_response`.
#[derive(Clone, Debug, Default)]
pub struct ProviderResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
}

/// Inspects or replaces the request payload before it is sent; `None` keeps it. Upstream
/// `onPayload` may return a promise, so the hook is asynchronous; see [`payload_hook`].
pub type OnPayloadFn = Arc<
    dyn Fn(&serde_json::Value, &Model) -> futures::future::BoxFuture<'static, Option<serde_json::Value>> + Send + Sync,
>;

/// An [`OnPayloadFn`] from a synchronous function.
pub fn payload_hook(
    hook: impl Fn(&serde_json::Value, &Model) -> Option<serde_json::Value> + Send + Sync + 'static,
) -> OnPayloadFn {
    Arc::new(move |payload, model| {
        let result = hook(payload, model);
        Box::pin(async move { result })
    })
}
pub type OnResponseFn = Arc<dyn Fn(&ProviderResponse, &Model) + Send + Sync>;
/// Observes each parsed provider stream event before normalization.
pub type OnProviderStreamEventFn = Arc<dyn Fn(&serde_json::Value, &Model) + Send + Sync>;

/// Per-request options. Field meanings follow upstream `StreamOptions` and `SimpleStreamOptions`.
/// `api_key` falls back to the provider's environment variable.
#[derive(Clone, Default)]
pub struct StreamOptions {
    pub api_key: Option<String>,
    pub cancel: CancellationToken,
    /// Explicit parent span for telemetry produced by this logical request.
    pub telemetry: Option<Arc<dyn TelemetrySpan>>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    /// Requested reasoning effort; `None` or `Off` disables thinking where the API allows it.
    pub reasoning: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Extra headers; a `None` value suppresses a provider default header.
    pub headers: BTreeMap<String, String>,
    pub suppressed_headers: Vec<String>,
    /// How the request identifies the calling app: `User-Agent` and provider attribution headers.
    /// Unset fields use pi-ai's defaults: a detailed `User-Agent` and no attribution.
    pub identity: Option<ClientIdentity>,
    pub session_id: Option<String>,
    /// Provider-scoped environment values that take precedence over the process environment.
    pub env: BTreeMap<String, String>,
    pub transport: Option<Transport>,
    pub cache_retention: Option<CacheRetention>,
    /// Merged into OpenAI-compatible request bodies after named fields, over `Model.sampling_params`.
    pub sampling_params: Option<serde_json::Map<String, serde_json::Value>>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    /// Longest server-requested retry delay to honor; 0 disables the cap. Default 60000.
    pub max_retry_delay_ms: Option<u64>,
    pub websocket_connect_timeout_ms: Option<u64>,
    /// `auto` or `none`; adapters choose when unset.
    pub tool_choice: Option<String>,
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    /// OpenAI `service_tier` (`auto`, `default`, `flex`, `priority`) for APIs that accept it.
    pub service_tier: Option<String>,
    /// Ask a capable provider for a durable deferred handle (`15m`, `1h`, `24h`).
    pub deferred: Option<String>,
    /// OpenAI Responses, Azure, and Codex reasoning summary; alone it requests `medium` effort
    /// on Responses and Azure.
    pub reasoning_summary: Option<ReasoningSummary>,
    pub vertex: VertexOptions,
    pub azure: AzureOptions,
    pub bedrock: BedrockOptions,
    /// pi-messages: ask the backend for debug metadata (`?debug=1`).
    pub debug: bool,
    pub on_payload: Option<OnPayloadFn>,
    pub on_response: Option<OnResponseFn>,
    pub on_provider_stream_event: Option<OnProviderStreamEventFn>,
}

impl std::fmt::Debug for StreamOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamOptions")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("max_tokens", &self.max_tokens)
            .field("reasoning", &self.reasoning)
            .field("session_id", &self.session_id)
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

/// Options for fetching or cancelling a deferred response.
#[derive(Clone, Debug, Default)]
pub struct DeferredOptions {
    pub request: RequestOptions,
    /// Longest provider long-poll in milliseconds; `None` or 0 checks once.
    pub wait_ms: Option<u64>,
}

/// One wire protocol, such as `openai-completions`. Implementations must
/// always end the returned stream with `Done` or `Error` and honor `options.cancel`.
pub trait ApiProvider: Send + Sync {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream;

    /// Whether `stream` reads system messages from the transcript itself. When false, the
    /// registry first folds them into `system_prompt` and `tools`.
    fn supports_system_messages(&self) -> bool {
        false
    }

    /// Continue a response that ended with stop reason `Deferred`. A response that is still
    /// pending ends with `Deferred` again.
    fn fetch_deferred(
        &self,
        model: &Model,
        _handle: &DeferredHandle,
        _options: DeferredOptions,
    ) -> AssistantMessageEventStream {
        error_stream(model, StopReason::Error, "API does not support deferred responses")
    }

    /// Best-effort cancellation of a deferred response.
    fn cancel_deferred(
        &self,
        _model: &Model,
        _handle: &DeferredHandle,
        _options: DeferredOptions,
    ) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async { Err("API cannot cancel deferred responses".to_string()) })
    }
}

type Registry = RwLock<HashMap<String, Arc<dyn ApiProvider>>>;

static REGISTRY: LazyLock<Registry> = LazyLock::new(|| {
    let mut providers: HashMap<String, Arc<dyn ApiProvider>> = HashMap::new();
    providers.insert("openai-completions".into(), Arc::new(providers::openai_completions::OpenAICompletions));
    providers.insert("openai-responses".into(), Arc::new(providers::openai_responses::OpenAIResponses));
    providers.insert("anthropic-messages".into(), Arc::new(providers::anthropic::AnthropicMessages));
    providers.insert("bedrock-converse-stream".into(), Arc::new(providers::bedrock::BedrockConverseStream));
    providers.insert("openai-codex-responses".into(), Arc::new(providers::openai_codex::OpenAICodexResponses));
    providers.insert("google-generative-ai".into(), Arc::new(providers::google::GoogleGenerativeAI));
    providers.insert("google-vertex".into(), Arc::new(providers::google_vertex::GoogleVertex));
    providers.insert("mistral-conversations".into(), Arc::new(providers::mistral::MistralConversations));
    providers.insert("pi-messages".into(), Arc::new(providers::pi_messages::PiMessages));
    providers
        .insert("azure-openai-responses".into(), Arc::new(providers::azure_openai_responses::AzureOpenAIResponses));
    RwLock::new(providers)
});

/// Register or replace the provider for `api`.
pub fn register_api_provider(api: impl Into<String>, provider: Arc<dyn ApiProvider>) {
    REGISTRY.write().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(api.into(), provider);
}

/// Remove the provider for `api`, returning it.
pub fn unregister_api_provider(api: &str) -> Option<Arc<dyn ApiProvider>> {
    REGISTRY.write().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(api)
}

pub fn api_provider(api: &str) -> Option<Arc<dyn ApiProvider>> {
    REGISTRY.read().unwrap_or_else(|poisoned| poisoned.into_inner()).get(api).cloned()
}

/// Stream a response from the provider registered for `model.api`, inside a `pi.ai.request` span under
/// `options.telemetry`.
pub fn stream(model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
    let (telemetry, reasoning) = (options.telemetry.clone(), options.reasoning);
    observe_model_request(model, telemetry, reasoning, false, dispatch_stream(model, context, options))
}

fn dispatch_stream(model: &Model, context: &Context, mut options: StreamOptions) -> AssistantMessageEventStream {
    let Some(provider) = api_provider(&model.api) else {
        return error_stream(model, StopReason::Error, format!("No provider registered for API: {}", model.api));
    };
    if options.api_key.is_none() {
        options.api_key = get_env_api_key(&model.provider);
    }
    add_opencode_session_header(model, &mut options);
    let (model, options) = providers::cloudflare::prepare_cloudflare_request(model, options);
    let model = model.as_ref();
    let has_system = context.messages.iter().any(|message| matches!(message, crate::types::Message::System(_)));
    if has_system && !provider.supports_system_messages() {
        return provider.stream(model, &crate::transcript::flatten_context(context), options);
    }
    provider.stream(model, context, options)
}

/// OpenCode routes a conversation by its `x-opencode-session` header; configured headers win.
fn add_opencode_session_header(model: &Model, options: &mut StreamOptions) {
    let Some((name, value)) = crate::identity::opencode_session_header(model, options.session_id.as_deref()) else {
        return;
    };
    if !options.headers.keys().any(|present| present.eq_ignore_ascii_case(name)) {
        options.headers.insert(name.to_string(), value);
    }
}

fn deferred_provider(model: &Model, options: &mut DeferredOptions) -> Result<Arc<dyn ApiProvider>, String> {
    if options.request.api_key.is_none() {
        options.request.api_key = get_env_api_key(&model.provider);
    }
    api_provider(&model.api).ok_or_else(|| format!("No provider registered for API: {}", model.api))
}

/// Stream the continuation of a deferred response, inside a `pi.ai.poll` span under `options.request.telemetry`.
pub fn stream_deferred(
    model: &Model,
    handle: &DeferredHandle,
    mut options: DeferredOptions,
) -> AssistantMessageEventStream {
    let telemetry = options.request.telemetry.clone();
    let inner = match deferred_provider(model, &mut options) {
        Ok(provider) => provider.fetch_deferred(model, handle, options),
        Err(error) => error_stream(model, StopReason::Error, error),
    };
    observe_model_request(model, telemetry, None, true, inner)
}

/// Fetch a deferred response; the result has stop reason `Deferred` while it is still pending.
pub async fn fetch_deferred(model: &Model, handle: &DeferredHandle, options: DeferredOptions) -> AssistantMessage {
    stream_deferred(model, handle, options).result(model).await
}

/// Best-effort cancellation of a deferred response.
pub async fn cancel_deferred(
    model: &Model,
    handle: &DeferredHandle,
    mut options: DeferredOptions,
) -> Result<(), String> {
    deferred_provider(model, &mut options)?.cancel_deferred(model, handle, options).await
}

/// Stream a response and return the final message.
pub async fn complete(model: &Model, context: &Context, options: StreamOptions) -> AssistantMessage {
    stream(model, context, options).result(model).await
}

/// Shared pooled HTTP client for providers; proxies and idle timeouts follow [`crate::http`].
pub fn http_client() -> reqwest::Client {
    crate::http::shared_client()
}
