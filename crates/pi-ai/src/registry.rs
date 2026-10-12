//! Provider registry and the top-level `stream` and `complete` entry points.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, RwLock};

use tokio_util::sync::CancellationToken;

use crate::env::get_env_api_key;
use crate::providers;
use crate::stream::{AssistantMessageEventStream, error_stream};
use crate::types::{AssistantMessage, Context, Model, StopReason, ThinkingLevel};

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

/// Inspects or replaces the request payload before it is sent; `None` keeps it.
pub type OnPayloadFn = Arc<dyn Fn(&serde_json::Value, &Model) -> Option<serde_json::Value> + Send + Sync>;
pub type OnResponseFn = Arc<dyn Fn(&ProviderResponse, &Model) + Send + Sync>;
/// Observes each parsed provider stream event before normalization.
pub type OnProviderStreamEventFn = Arc<dyn Fn(&serde_json::Value, &Model) + Send + Sync>;

/// Per-request options. Field meanings follow upstream `StreamOptions` and `SimpleStreamOptions`.
/// `api_key` falls back to the provider's environment variable.
#[derive(Clone, Default)]
pub struct StreamOptions {
    pub api_key: Option<String>,
    pub cancel: CancellationToken,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    /// Requested reasoning effort; `None` or `Off` disables thinking where the API allows it.
    pub reasoning: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Extra headers; a `None` value suppresses a provider default header.
    pub headers: BTreeMap<String, String>,
    pub suppressed_headers: Vec<String>,
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
    /// Ask a capable provider for a durable deferred handle (`15m`, `1h`, `24h`).
    pub deferred: Option<String>,
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

/// One wire protocol, such as `openai-completions`. Implementations must
/// always end the returned stream with `Done` or `Error` and honor `options.cancel`.
pub trait ApiProvider: Send + Sync {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream;
}

type Registry = RwLock<HashMap<String, Arc<dyn ApiProvider>>>;

static REGISTRY: LazyLock<Registry> = LazyLock::new(|| {
    let mut providers: HashMap<String, Arc<dyn ApiProvider>> = HashMap::new();
    providers.insert("openai-completions".into(), Arc::new(providers::openai_completions::OpenAICompletions));
    providers.insert("openai-responses".into(), Arc::new(providers::openai_responses::OpenAIResponses));
    providers.insert("anthropic-messages".into(), Arc::new(providers::anthropic::AnthropicMessages));
    providers.insert("bedrock-converse-stream".into(), Arc::new(providers::bedrock::BedrockConverseStream));
    RwLock::new(providers)
});

/// Register or replace the provider for `api`.
pub fn register_api_provider(api: impl Into<String>, provider: Arc<dyn ApiProvider>) {
    REGISTRY.write().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(api.into(), provider);
}

pub fn api_provider(api: &str) -> Option<Arc<dyn ApiProvider>> {
    REGISTRY.read().unwrap_or_else(|poisoned| poisoned.into_inner()).get(api).cloned()
}

/// Stream a response from the provider registered for `model.api`.
pub fn stream(model: &Model, context: &Context, mut options: StreamOptions) -> AssistantMessageEventStream {
    let Some(provider) = api_provider(&model.api) else {
        return error_stream(model, StopReason::Error, format!("No provider registered for API: {}", model.api));
    };
    if options.api_key.is_none() {
        options.api_key = get_env_api_key(&model.provider);
    }
    if context.messages.iter().any(|message| matches!(message, crate::types::Message::System(_))) {
        return provider.stream(model, &crate::transcript::flatten_context(context), options);
    }
    provider.stream(model, context, options)
}

/// Stream a response and return the final message.
pub async fn complete(model: &Model, context: &Context, options: StreamOptions) -> AssistantMessage {
    stream(model, context, options).result(model).await
}

/// Shared HTTP client for providers.
pub fn http_client() -> &'static reqwest::Client {
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .user_agent(concat!("pi-rust/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("HTTP client configuration is valid")
    });
    &CLIENT
}
