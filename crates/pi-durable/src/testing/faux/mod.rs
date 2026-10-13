//! Scripted [`Models`] for tests: a port of upstream pi-ai `providers/faux.ts` (`fauxProvider`). Queue responses
//! with [`FauxProviderHandle::set_responses`]; each request takes the next one and streams it token by token. Pass
//! `handle.provider()` to a harness, or to `RegistryModels::set_provider` as upstream `models.setProvider` does.

mod core;
mod stream;
mod usage;

use std::sync::Arc;

use futures::future::BoxFuture;
use pi_ai::{
    AssistantMessage, ContentBlock, Context, DeferredHandle, JsonObject, Model, ModelCost, ModelInputLimits,
    StopReason, ToolCall, Usage, now_ms,
};

pub use self::core::FauxProvider;
use crate::models::{Models, SimpleStreamOptions};

pub const DEFAULT_API: &str = "faux";
pub const DEFAULT_PROVIDER: &str = "faux";
pub const DEFAULT_MODEL_ID: &str = "faux-1";
pub const DEFAULT_MODEL_NAME: &str = "Faux Model";
pub const DEFAULT_BASE_URL: &str = "http://localhost:0";
const DEFAULT_MIN_TOKEN_SIZE: u32 = 3;
const DEFAULT_MAX_TOKEN_SIZE: u32 = 5;

/// One model of a faux provider; unset fields take the upstream defaults.
#[derive(Clone, Debug, Default)]
pub struct FauxModelDefinition {
    pub id: String,
    pub name: Option<String>,
    pub reasoning: Option<bool>,
    /// `text`, `image`.
    pub input: Option<Vec<String>>,
    pub input_limits: Option<ModelInputLimits>,
    pub cost: Option<ModelCost>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
}

impl FauxModelDefinition {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into(), ..Self::default() }
    }
}

/// Deferred behavior: fetches that still return the handle before the response is ready, and its poll hint.
#[derive(Clone, Copy, Debug, Default)]
pub struct FauxDeferredOptions {
    pub pending_fetches: u32,
    pub poll_after_ms: Option<u64>,
}

/// Chunk size bounds in tokens (default 3 to 5).
#[derive(Clone, Copy, Debug, Default)]
pub struct FauxTokenSize {
    pub min: Option<u32>,
    pub max: Option<u32>,
}

/// Upstream `RegisterFauxProviderOptions`.
#[derive(Clone, Debug, Default)]
pub struct RegisterFauxProviderOptions {
    /// Default: a random `faux:<time>:<random>` id.
    pub api: Option<String>,
    /// Default: `faux`.
    pub provider: Option<String>,
    /// Default: one `faux-1` model.
    pub models: Vec<FauxModelDefinition>,
    pub deferred: Option<FauxDeferredOptions>,
    /// Pacing of streamed chunks; unset or 0 streams without delay.
    pub tokens_per_second: Option<f64>,
    pub token_size: Option<FauxTokenSize>,
}

/// Request counters. `cancelled_deferred` lists the handles passed to `cancel_deferred`, in order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FauxProviderState {
    pub call_count: u32,
    pub deferred_fetch_count: u32,
    pub cancelled_deferred: Vec<DeferredHandle>,
}

/// Builds a response from the request. `Err` becomes an `error` stream, as a thrown error does upstream.
pub type FauxResponseFactory = Arc<
    dyn Fn(
            &Context,
            &SimpleStreamOptions,
            &FauxProviderState,
            &Model,
        ) -> BoxFuture<'static, Result<AssistantMessage, String>>
        + Send
        + Sync,
>;

/// One queued response: a message, or a factory called with the request.
#[derive(Clone)]
pub enum FauxResponseStep {
    Message(Box<AssistantMessage>),
    Factory(FauxResponseFactory),
}

impl FauxResponseStep {
    /// A synchronous factory.
    pub fn factory(
        build: impl Fn(&Context, &SimpleStreamOptions, &FauxProviderState, &Model) -> AssistantMessage
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self::try_factory(move |context, options, state, model| Ok(build(context, options, state, model)))
    }

    /// A synchronous factory that may fail.
    pub fn try_factory(
        build: impl Fn(&Context, &SimpleStreamOptions, &FauxProviderState, &Model) -> Result<AssistantMessage, String>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self::Factory(Arc::new(move |context, options, state, model| {
            let result = build(context, options, state, model);
            Box::pin(async move { result })
        }))
    }

    /// An asynchronous factory; the closure clones what its future needs.
    pub fn async_factory(
        build: impl Fn(
            &Context,
            &SimpleStreamOptions,
            &FauxProviderState,
            &Model,
        ) -> BoxFuture<'static, Result<AssistantMessage, String>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self::Factory(Arc::new(build))
    }
}

impl From<AssistantMessage> for FauxResponseStep {
    fn from(message: AssistantMessage) -> Self {
        Self::Message(Box::new(message))
    }
}

impl std::fmt::Debug for FauxResponseStep {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(message) => formatter.debug_tuple("Message").field(message).finish(),
            Self::Factory(_) => formatter.write_str("Factory"),
        }
    }
}

pub fn faux_text(text: impl Into<String>) -> ContentBlock {
    ContentBlock::text(text)
}

pub fn faux_thinking(thinking: impl Into<String>) -> ContentBlock {
    ContentBlock::Thinking { thinking: thinking.into(), thinking_signature: None, redacted: false }
}

/// A tool call with a random `tool:<time>:<random>` id. `arguments` should be a JSON object; anything else is `{}`.
pub fn faux_tool_call(name: impl Into<String>, arguments: serde_json::Value) -> ContentBlock {
    faux_tool_call_with_id(name, arguments, core::random_id("tool"))
}

pub fn faux_tool_call_with_id(
    name: impl Into<String>,
    arguments: serde_json::Value,
    id: impl Into<String>,
) -> ContentBlock {
    let arguments: JsonObject = match arguments {
        serde_json::Value::Object(arguments) => arguments,
        _ => JsonObject::new(),
    };
    ContentBlock::ToolCall(ToolCall { id: id.into(), name: name.into(), arguments, ..ToolCall::default() })
}

/// Content of a faux message: a string, one block, or several.
pub struct FauxContent(pub Vec<ContentBlock>);

impl From<&str> for FauxContent {
    fn from(text: &str) -> Self {
        Self(vec![faux_text(text)])
    }
}

impl From<String> for FauxContent {
    fn from(text: String) -> Self {
        Self(vec![faux_text(text)])
    }
}

impl From<ContentBlock> for FauxContent {
    fn from(block: ContentBlock) -> Self {
        Self(vec![block])
    }
}

impl From<Vec<ContentBlock>> for FauxContent {
    fn from(blocks: Vec<ContentBlock>) -> Self {
        Self(blocks)
    }
}

/// Options of [`faux_assistant_message`]; the stop reason defaults to `Stop`, the timestamp to now.
#[derive(Clone, Debug, Default)]
pub struct FauxMessageOptions {
    pub stop_reason: Option<StopReason>,
    pub deferred: Option<DeferredHandle>,
    pub error_message: Option<String>,
    pub response_id: Option<String>,
    pub timestamp: Option<i64>,
}

impl FauxMessageOptions {
    pub fn stop_reason(stop_reason: StopReason) -> Self {
        Self { stop_reason: Some(stop_reason), ..Self::default() }
    }
}

/// Upstream `fauxAssistantMessage`.
pub fn faux_assistant_message(content: impl Into<FauxContent>, options: FauxMessageOptions) -> AssistantMessage {
    AssistantMessage {
        content: content.into().0,
        api: DEFAULT_API.into(),
        provider: DEFAULT_PROVIDER.into(),
        model: DEFAULT_MODEL_ID.into(),
        usage: Usage::default(),
        stop_reason: options.stop_reason.unwrap_or(StopReason::Stop),
        deferred: options.deferred,
        error_message: options.error_message,
        response_id: options.response_id,
        timestamp: options.timestamp.unwrap_or_else(now_ms),
        ..AssistantMessage::default()
    }
}

/// Upstream `FauxProviderHandle`: the provider plus its models and script controls.
#[derive(Clone)]
pub struct FauxProviderHandle {
    core: Arc<FauxProvider>,
    pub api: String,
    pub models: Vec<Model>,
}

impl FauxProviderHandle {
    /// The provider as [`Models`], for a harness or `RegistryModels::set_provider`.
    pub fn provider(&self) -> Arc<dyn Models> {
        self.core.clone()
    }

    /// The provider id its models carry (`faux` by default).
    pub fn provider_id(&self) -> &str {
        self.core.provider_id()
    }

    /// The first model (upstream `getModel()`).
    pub fn get_model(&self) -> Model {
        self.models[0].clone()
    }

    /// The model with `id` (upstream `getModel(id)`).
    pub fn get_model_by_id(&self, id: &str) -> Option<Model> {
        self.models.iter().find(|model| model.id == id).cloned()
    }

    /// A snapshot of the request counters.
    pub fn state(&self) -> FauxProviderState {
        self.core.state()
    }

    /// Replace the queued responses.
    pub fn set_responses<S: Into<FauxResponseStep>>(&self, responses: impl IntoIterator<Item = S>) {
        self.core.set_responses(responses.into_iter().map(Into::into).collect());
    }

    /// Queue more responses after the current ones.
    pub fn append_responses<S: Into<FauxResponseStep>>(&self, responses: impl IntoIterator<Item = S>) {
        self.core.append_responses(responses.into_iter().map(Into::into).collect());
    }

    pub fn get_pending_response_count(&self) -> usize {
        self.core.pending_response_count()
    }
}

fn model_from_definition(definition: FauxModelDefinition, api: &str, provider: &str) -> Model {
    Model {
        name: definition.name.unwrap_or_else(|| definition.id.clone()),
        id: definition.id,
        api: api.into(),
        provider: provider.into(),
        base_url: DEFAULT_BASE_URL.into(),
        reasoning: definition.reasoning.unwrap_or(false),
        input: definition.input.unwrap_or_else(|| vec!["text".into(), "image".into()]),
        input_limits: definition.input_limits,
        cost: definition.cost.unwrap_or_default(),
        context_window: definition.context_window.unwrap_or(128_000),
        max_tokens: definition.max_tokens.unwrap_or(16_384),
        ..Model::default()
    }
}

/// Upstream `fauxProvider(options)`.
pub fn faux_provider(options: RegisterFauxProviderOptions) -> FauxProviderHandle {
    let api = options.api.clone().unwrap_or_else(|| core::random_id(DEFAULT_API));
    let provider = options.provider.clone().unwrap_or_else(|| DEFAULT_PROVIDER.into());
    let size = options.token_size.unwrap_or_default();
    let min_token_size =
        size.min.unwrap_or(DEFAULT_MIN_TOKEN_SIZE).min(size.max.unwrap_or(DEFAULT_MAX_TOKEN_SIZE)).max(1);
    let max_token_size = size.max.unwrap_or(DEFAULT_MAX_TOKEN_SIZE).max(min_token_size);
    let definitions = if options.models.is_empty() {
        vec![FauxModelDefinition {
            id: DEFAULT_MODEL_ID.into(),
            name: Some(DEFAULT_MODEL_NAME.into()),
            ..FauxModelDefinition::default()
        }]
    } else {
        options.models.clone()
    };
    let models: Vec<Model> =
        definitions.into_iter().map(|definition| model_from_definition(definition, &api, &provider)).collect();
    let pacing = stream::Pacing { min_token_size, max_token_size, tokens_per_second: options.tokens_per_second };
    let core = Arc::new(FauxProvider::new(api.clone(), provider, models.clone(), pacing, options.deferred));
    FauxProviderHandle { core, api, models }
}
