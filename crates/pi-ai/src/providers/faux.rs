//! Scripted provider for tests and offline demos. Register it with
//! `register_api_provider("faux", provider)` and use `FauxProvider::model()`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::registry::{ApiProvider, StreamOptions};
use crate::stream::{AssistantMessageEventStream, MessageBuilder, event_channel};
use crate::types::{ContentBlock, Context, Model, ModelCost, StopReason, Usage};

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

#[derive(Default)]
pub struct FauxProvider {
    responses: Mutex<VecDeque<FauxResponse>>,
    requests: Mutex<Vec<Context>>,
    chunk_delay: Duration,
}

impl FauxProvider {
    pub fn new(responses: impl IntoIterator<Item = FauxResponse>) -> Arc<Self> {
        Arc::new(Self { responses: Mutex::new(responses.into_iter().collect()), ..Self::default() })
    }

    /// Wait `delay` between streamed chunks.
    pub fn with_delay(responses: impl IntoIterator<Item = FauxResponse>, delay: Duration) -> Arc<Self> {
        Arc::new(Self { responses: Mutex::new(responses.into_iter().collect()), chunk_delay: delay, ..Self::default() })
    }

    pub fn push(&self, response: FauxResponse) {
        self.responses.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push_back(response);
    }

    /// Contexts received so far, in order.
    pub fn requests(&self) -> Vec<Context> {
        self.requests.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
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
}

impl ApiProvider for FauxProvider {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        self.requests.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(context.clone());
        let response = self.responses.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).pop_front();
        let (sender, stream) = event_channel();
        let mut builder = MessageBuilder::new(model, sender);
        let delay = self.chunk_delay;
        let cancel = options.cancel.clone();
        let input_tokens = serde_json::to_string(context).map(|json| json.len() as u64 / 4).unwrap_or_default();
        let cost = model.cost.clone();
        tokio::spawn(async move {
            builder.start();
            let pause = || async {
                if !delay.is_zero() {
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = cancel.cancelled() => {}
                    }
                }
            };
            let Some(response) = response else {
                builder.fail(StopReason::Error, "No scripted faux response");
                return;
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
        });
        stream
    }
}

fn chunks(text: &str) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    characters.chunks(8).map(|chunk| chunk.iter().collect()).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::registry::register_api_provider;

    #[tokio::test]
    async fn streams_scripted_text_and_tool_calls() {
        let provider = FauxProvider::new([
            FauxResponse::text("Hello from the faux provider"),
            FauxResponse::tool_call("c1", "read", json!({"path": "README.md"})),
        ]);
        register_api_provider("faux-test", provider.clone());
        let mut model = FauxProvider::model();
        model.api = "faux-test".into();
        let context = Context { messages: vec![crate::types::Message::user("hi")], ..Context::default() };
        let first = crate::complete(&model, &context, StreamOptions::default()).await;
        assert_eq!(first.text(), "Hello from the faux provider");
        assert_eq!(first.stop_reason, StopReason::Stop);
        let second = crate::complete(&model, &context, StreamOptions::default()).await;
        assert_eq!(second.stop_reason, StopReason::ToolUse);
        assert_eq!(second.tool_calls().next().unwrap().arguments["path"], "README.md");
        assert_eq!(provider.requests().len(), 2);
    }

    #[tokio::test]
    async fn hang_ends_with_abort_when_cancelled() {
        let provider = FauxProvider::new([FauxResponse::Hang]);
        let options = StreamOptions::default();
        let cancel = options.cancel.clone();
        let stream = provider.stream(&FauxProvider::model(), &Context::default(), options);
        cancel.cancel();
        assert_eq!(stream.result(&FauxProvider::model()).await.stop_reason, StopReason::Aborted);
    }
}
