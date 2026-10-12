//! Placeholder until the protocol is implemented.

use crate::registry::{ApiProvider, StreamOptions};
use crate::stream::{AssistantMessageEventStream, error_stream};
use crate::types::{Context, Model, StopReason};

pub struct OpenAICompletions;

impl ApiProvider for OpenAICompletions {
    fn stream(&self, model: &Model, _context: &Context, _options: StreamOptions) -> AssistantMessageEventStream {
        error_stream(model, StopReason::Error, "Not implemented")
    }
}
