//! Model type checks and error results for the non-chat operations (upstream
//! `utils/model-operations.ts`).

use crate::classify::{ClassifierContext, ClassifierResult};
use crate::images::{AssistantImages, ImagesStopReason};
use crate::types::{Model, now_ms};

/// The type of a model: `chat`, `image`, or `classifier`. Models without `type` are chat models.
pub fn model_type(model: &Model) -> &str {
    model.model_type.as_deref().unwrap_or("chat")
}

pub fn is_model_type(model: &Model, kind: &str) -> bool {
    model_type(model) == kind
}

fn assert_model_type(model: &Model, kind: &str, noun: &str) -> Result<(), String> {
    match is_model_type(model, kind) {
        true => Ok(()),
        false => Err(format!("Model {}/{} is not {noun}", model.provider, model.id)),
    }
}

pub fn assert_chat_model(model: &Model) -> Result<(), String> {
    assert_model_type(model, "chat", "a chat model")
}

pub fn assert_image_model(model: &Model) -> Result<(), String> {
    assert_model_type(model, "image", "an image model")
}

pub fn assert_classifier_model(model: &Model) -> Result<(), String> {
    assert_model_type(model, "classifier", "a classifier model")
}

/// Rejects classifier images for models whose catalog entry does not accept image input.
pub fn assert_classifier_input_supported(model: &Model, context: &ClassifierContext) -> Result<(), String> {
    match context.has_images() && !model.supports_images() {
        true => Err(format!("Model {}/{} does not accept image input", model.provider, model.id)),
        false => Ok(()),
    }
}

pub fn image_error_result(model: &Model, message: impl Into<String>, aborted: bool) -> AssistantImages {
    AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: Vec::new(),
        response_id: None,
        usage: None,
        stop_reason: if aborted { ImagesStopReason::Aborted } else { ImagesStopReason::Error },
        error_message: Some(message.into()),
        timestamp: now_ms(),
    }
}

pub fn classifier_error_result(model: &Model, message: impl Into<String>, aborted: bool) -> ClassifierResult {
    let mut result = ClassifierResult::new(model);
    result.fail(message, aborted);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::ImageContent;

    #[test]
    fn checks_model_types_and_image_input() {
        let mut model = Model { id: "m".into(), provider: "p".into(), ..Model::default() };
        assert_eq!(model_type(&model), "chat");
        assert!(assert_chat_model(&model).is_ok());
        assert_eq!(assert_classifier_model(&model).unwrap_err(), "Model p/m is not a classifier model");
        assert_eq!(assert_image_model(&model).unwrap_err(), "Model p/m is not an image model");
        model.model_type = Some("classifier".into());
        assert!(assert_classifier_model(&model).is_ok());

        let image = ImageContent { data: "aW1hZ2U=".into(), mime_type: "image/png".into() };
        let mut context = ClassifierContext { images: Some(Vec::new()), ..ClassifierContext::default() };
        assert!(assert_classifier_input_supported(&model, &context).is_ok());
        context.images = Some(vec![image]);
        model.input = vec!["text".into()];
        assert_eq!(
            assert_classifier_input_supported(&model, &context).unwrap_err(),
            "Model p/m does not accept image input"
        );
        model.input.push("image".into());
        assert!(assert_classifier_input_supported(&model, &context).is_ok());

        let failed = classifier_error_result(&model, "boom", true);
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!((json["stopReason"].as_str(), json["errorMessage"].as_str()), (Some("aborted"), Some("boom")));
        let failed = image_error_result(&model, "boom", false);
        assert_eq!(serde_json::to_value(&failed).unwrap()["stopReason"], "error");
    }
}
