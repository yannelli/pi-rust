//! Image generation with image models: a registry of image APIs keyed by `model.api` and a
//! `generate_images` entry point that reports failures in its result (upstream `images.ts`).

mod openrouter;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use openrouter::OpenRouterImages;

use crate::env::get_env_api_key;
use crate::model_operations::{assert_image_model, image_error_result};
use crate::request::RequestOptions;
use crate::types::{ContentBlock, Model, Usage};

/// The prompt: text and image blocks (`ContentBlock::Text` and `ContentBlock::Image`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImagesContext {
    pub input: Vec<ContentBlock>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImagesStopReason {
    #[default]
    Stop,
    Error,
    Aborted,
}

/// The result of one image generation request (upstream `AssistantImages`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantImages {
    pub api: String,
    pub provider: String,
    pub model: String,
    /// Text and image blocks in the order the provider returned them.
    pub output: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub stop_reason: ImagesStopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
}

impl AssistantImages {
    /// An empty successful result for `model`, stamped now.
    pub fn new(model: &Model) -> Self {
        Self {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            timestamp: crate::types::now_ms(),
            ..Self::default()
        }
    }
}

/// Options for one image generation request (upstream `ImagesOptions`).
#[derive(Clone, Debug, Default)]
pub struct ImagesOptions {
    /// Auth, transport, and hooks; `api_key` falls back to the provider's environment variable.
    pub request: RequestOptions,
    /// Request metadata; providers use the fields they understand and ignore the rest.
    pub metadata: Option<serde_json::Map<String, Value>>,
}

/// One image generation API, such as `openrouter-images`. Implementations report every failure
/// in the returned result (`error`, or `aborted` after cancellation) and never panic.
pub trait ImagesApi: Send + Sync {
    fn generate_images(
        &self,
        model: &Model,
        context: &ImagesContext,
        options: ImagesOptions,
    ) -> BoxFuture<'static, AssistantImages>;
}

type Registry = RwLock<HashMap<String, Arc<dyn ImagesApi>>>;

static REGISTRY: LazyLock<Registry> = LazyLock::new(|| {
    let mut apis: HashMap<String, Arc<dyn ImagesApi>> = HashMap::new();
    apis.insert("openrouter-images".into(), Arc::new(OpenRouterImages));
    RwLock::new(apis)
});

/// Register or replace the image generation implementation for `api`.
pub fn register_images_api(api: impl Into<String>, provider: Arc<dyn ImagesApi>) {
    REGISTRY.write().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(api.into(), provider);
}

pub fn images_api(api: &str) -> Option<Arc<dyn ImagesApi>> {
    REGISTRY.read().unwrap_or_else(|poisoned| poisoned.into_inner()).get(api).cloned()
}

/// Generate images with the API registered for `model.api`. Failures come back as results
/// with `stopReason` `error`, or `aborted` when `options.request.cancel` fired.
pub async fn generate_images(model: &Model, context: &ImagesContext, mut options: ImagesOptions) -> AssistantImages {
    let aborted = || options.request.cancel.is_cancelled();
    if let Err(message) = assert_image_model(model) {
        return image_error_result(model, message, aborted());
    }
    let Some(api) = images_api(&model.api) else {
        return image_error_result(model, format!("No API provider registered for api: {}", model.api), aborted());
    };
    if options.request.api_key.is_none() {
        options.request.api_key = get_env_api_key(&model.provider);
    }
    api.generate_images(model, context, options).await
}
