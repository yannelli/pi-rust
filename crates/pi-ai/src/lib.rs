//! Unified streaming LLM API for the pi agent harness.

pub mod env;
pub mod json;
pub mod models;
pub mod providers;
pub mod registry;
pub mod sse;
pub mod stream;
pub mod types;

pub use env::get_env_api_key;
pub use models::ModelRegistry;
pub use registry::{ApiProvider, StreamOptions, api_provider, complete, http_client, register_api_provider, stream};
pub use stream::{AssistantMessageEvent, AssistantMessageEventStream, EventSender, MessageBuilder, error_stream, event_channel};
pub use types::*;
