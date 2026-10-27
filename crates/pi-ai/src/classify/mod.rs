//! Structured classification with classifier models: a registry of classifier wire protocols
//! keyed by `model.api` and a `classify` entry point that reports failures in its result.

mod llama_cpp;
mod llama_render;
mod openai_decisions;
mod ordered_map;
pub(crate) mod shared;
mod system_one;
mod types;

#[cfg(test)]
mod cloudflare_tests;
#[cfg(test)]
mod entry_tests;
#[cfg(test)]
mod llama_cpp_tests;
#[cfg(test)]
mod openai_decisions_tests;
#[cfg(test)]
mod system_one_image_tests;
#[cfg(test)]
mod system_one_tests;
#[cfg(test)]
mod test_support;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

use futures::future::BoxFuture;

pub use llama_cpp::LlamaCppClassify;
pub use openai_decisions::OpenAIDecisions;
pub use ordered_map::OrderedMap;
pub use system_one::{CloudflareWorkersAISystemOne, TypeSafeSystemOne};
pub use types::{
    BoolCriteria, ClassifierAnswer, ClassifierContext, ClassifierQuestion, ClassifierResult, ClassifierStopReason,
    ImageContent,
};

use crate::env::get_env_api_key;
use crate::model_operations::{assert_classifier_input_supported, assert_classifier_model, classifier_error_result};
use crate::request::RequestOptions;
use crate::types::Model;

/// Options for one classification (upstream `ClassifierOptions`).
#[derive(Clone, Debug, Default)]
pub struct ClassifierOptions {
    /// Auth, transport, and hooks; `api_key` falls back to the provider's environment variable.
    pub request: RequestOptions,
    /// Divides the answer logits before they are normalized into probabilities; values above 1
    /// soften the distribution. Must be positive. APIs that cannot apply it ignore it.
    pub temperature: Option<f64>,
}

/// One classifier wire protocol, such as `typesafe-system-one`. Implementations report every
/// failure in the returned result (`error`, or `aborted` after cancellation) and never panic.
pub trait ClassifierApi: Send + Sync {
    fn classify(
        &self,
        model: &Model,
        context: &ClassifierContext,
        options: ClassifierOptions,
    ) -> BoxFuture<'static, ClassifierResult>;
}

type Registry = RwLock<HashMap<String, Arc<dyn ClassifierApi>>>;

static REGISTRY: LazyLock<Registry> = LazyLock::new(|| {
    let mut apis: HashMap<String, Arc<dyn ClassifierApi>> = HashMap::new();
    apis.insert("typesafe-system-one".into(), Arc::new(TypeSafeSystemOne));
    apis.insert("cloudflare-workers-ai-system-one".into(), Arc::new(CloudflareWorkersAISystemOne));
    apis.insert("openai-decisions".into(), Arc::new(OpenAIDecisions));
    apis.insert("llama-cpp-classify".into(), Arc::new(LlamaCppClassify));
    RwLock::new(apis)
});

/// Register or replace the classifier implementation for `api`.
pub fn register_classifier_api(api: impl Into<String>, provider: Arc<dyn ClassifierApi>) {
    REGISTRY.write().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(api.into(), provider);
}

pub fn classifier_api(api: &str) -> Option<Arc<dyn ClassifierApi>> {
    REGISTRY.read().unwrap_or_else(|poisoned| poisoned.into_inner()).get(api).cloned()
}

/// Classify `context` with the classifier registered for `model.api`. Failures come back as
/// results with `stopReason` `error`, or `aborted` when `options.request.cancel` fired.
pub async fn classify(model: &Model, context: &ClassifierContext, mut options: ClassifierOptions) -> ClassifierResult {
    let aborted = || options.request.cancel.is_cancelled();
    if let Err(message) =
        assert_classifier_model(model).and_then(|()| assert_classifier_input_supported(model, context))
    {
        return classifier_error_result(model, message, aborted());
    }
    let Some(api) = classifier_api(&model.api) else {
        return classifier_error_result(model, format!("No API provider registered for api: {}", model.api), aborted());
    };
    if options.request.api_key.is_none() {
        options.request.api_key = get_env_api_key(&model.provider);
    }
    api.classify(model, context, options).await
}
