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

fn split_model_label(value: &str) -> Vec<String> {
    value
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(String::from)
        .collect()
}

/// The model name when it has words the id lacks, such as a renamed override like
/// "MiniMax M3 [parasail]" for "minimax/minimax-m3" (upstream `getDistinctModelName`). Names that
/// only restate the id with other casing, separators, or a dropped namespace or version stay hidden.
pub fn distinct_model_name(id: &str, name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let id_words = split_model_label(id);
    let compact_id = id_words.concat();
    // Single-character words must match a whole id token: "4" is not covered by "mistral-small-2603".
    let covered = |word: &String| {
        id_words.contains(word) || (word.encode_utf16().count() > 1 && compact_id.contains(word.as_str()))
    };
    (!split_model_label(name).iter().all(covered)).then(|| name.to_string())
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
