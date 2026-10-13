//! Unified streaming LLM API for the pi agent harness.

pub mod attribution;
pub mod auth;
pub mod classify;
pub mod env;
pub mod estimate;
pub mod http;
pub mod images;
pub mod json;
pub mod model_operations;
pub mod models;
pub mod overflow;
pub mod providers;
pub mod registry;
pub mod request;
pub mod retry;
pub mod simple_options;
pub mod sse;
pub mod stream;
pub mod transcript;
pub mod types;

pub use classify::{
    ClassifierApi, ClassifierContext, ClassifierOptions, ClassifierResult, classifier_api, classify,
    register_classifier_api,
};
pub use env::get_env_api_key;
pub use images::{
    AssistantImages, ImagesApi, ImagesContext, ImagesOptions, generate_images, images_api, register_images_api,
};
pub use models::ModelRegistry;
pub use overflow::{is_context_overflow, is_recoverable_length};
pub use registry::{
    ApiProvider, CacheRetention, DeferredOptions, ProviderResponse, StreamOptions, ThinkingBudgets, Transport,
    api_provider, cancel_deferred, complete, fetch_deferred, http_client, register_api_provider, stream,
    stream_deferred,
};
pub use request::RequestOptions;
pub use stream::{
    AssistantMessageEvent, AssistantMessageEventStream, EventSender, MessageBuilder, error_stream, event_channel,
};
pub use types::*;
