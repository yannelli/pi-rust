//! Builders shared by the classifier tests.

use serde_json::Value;

use super::{ClassifierContext, ClassifierOptions};
use crate::request::RequestOptions;
use crate::types::{Model, ModelCost};

pub(crate) fn classifier_model(api: &str, provider: &str, id: &str, base_url: &str) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: api.into(),
        provider: provider.into(),
        base_url: base_url.into(),
        model_type: Some("classifier".into()),
        input: vec!["text".into()],
        cost: ModelCost::default(),
        context_window: 64_000,
        ..Model::default()
    }
}

/// A context from its upstream JSON shape.
pub(crate) fn context(json: Value) -> ClassifierContext {
    serde_json::from_value(json).expect("valid classifier context")
}

pub(crate) fn with_key(api_key: &str) -> ClassifierOptions {
    ClassifierOptions {
        request: RequestOptions { api_key: Some(api_key.into()), ..RequestOptions::default() },
        temperature: None,
    }
}

/// The answers of a result as JSON, for comparisons with upstream shapes.
pub(crate) fn answers_json(result: &super::ClassifierResult) -> Value {
    serde_json::to_value(&result.answers).expect("answers serialize")
}
