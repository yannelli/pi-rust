//! Provider registry and the top-level `stream` and `complete` entry points.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, RwLock};

use tokio_util::sync::CancellationToken;

use crate::env::get_env_api_key;
use crate::providers;
use crate::stream::{AssistantMessageEventStream, error_stream};
use crate::types::{AssistantMessage, Context, Model, StopReason, ThinkingLevel};

/// Per-request options. `api_key` falls back to the provider's environment variable.
#[derive(Clone, Debug, Default)]
pub struct StreamOptions {
    pub api_key: Option<String>,
    pub cancel: CancellationToken,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    /// Requested reasoning effort; `None` or `Off` disables thinking where the API allows it.
    pub reasoning: Option<ThinkingLevel>,
    pub headers: BTreeMap<String, String>,
    pub session_id: Option<String>,
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
