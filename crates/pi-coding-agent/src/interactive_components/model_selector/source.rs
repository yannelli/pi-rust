//! What the model selector lists and how it refreshes: [`ModelSelectorSource`] (implemented by
//! [`ModelRuntime`]), the refresh hook, and the search text.

use std::future::Future;
use std::pin::Pin;

use pi_ai::Model;
use tokio_util::sync::CancellationToken;

use crate::agent_session::ModelRuntime;

/// The models the selector lists (upstream `ModelRuntime` snapshot accessors).
pub trait ModelSelectorSource {
    /// Models whose provider has usable auth.
    fn available_models(&self) -> Vec<Model>;
    fn find_model(&self, provider: &str, id: &str) -> Option<Model>;
    /// A catalog error to show instead of the selected model's name.
    fn catalog_error(&self) -> Option<String>;
}

impl ModelSelectorSource for ModelRuntime {
    fn available_models(&self) -> Vec<Model> {
        ModelRuntime::available_models(self)
    }

    fn find_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.get_model(provider, id)
    }

    fn catalog_error(&self) -> Option<String> {
        self.load_error().map(str::to_string)
    }
}

/// Outcome of a catalog refresh: whether it was aborted and the providers that failed, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelsRefreshResult {
    pub aborted: bool,
    pub errors: Vec<String>,
}

/// Starts a catalog refresh that stops when the token fires. An `Err` is a thrown error.
pub type ModelRefreshFn =
    Box<dyn FnOnce(CancellationToken) -> Pin<Box<dyn Future<Output = Result<ModelsRefreshResult, String>>>>>;

/// The provider and id of the default model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultModelReference {
    pub provider: String,
    pub id: String,
}

impl<T: ModelSelectorSource + ?Sized> ModelSelectorSource for std::sync::Arc<T> {
    fn available_models(&self) -> Vec<Model> {
        (**self).available_models()
    }

    fn find_model(&self, provider: &str, id: &str) -> Option<Model> {
        (**self).find_model(provider, id)
    }

    fn catalog_error(&self) -> Option<String> {
        (**self).catalog_error()
    }
}

/// Upstream `getModelSelectorSearchText`.
pub fn model_selector_search_text(model: &Model) -> String {
    let name = if model.name.is_empty() { String::new() } else { format!(" {}", model.name) };
    let (id, provider) = (&model.id, &model.provider);
    format!("{provider} {provider}/{id} {provider} {id}{name}")
}

/// Upstream `getModelSearchText`: the bare id first, for selectors that rank ids.
pub fn model_search_text(model: &Model) -> String {
    let name = if model.name.is_empty() { String::new() } else { format!(" {}", model.name) };
    let (id, provider) = (&model.id, &model.provider);
    format!("{id} {provider} {provider}/{id} {provider} {id}{name}")
}
