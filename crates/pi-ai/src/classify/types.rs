//! Classifier questions, answers, and results. JSON shapes match upstream `types.ts`.

use serde::{Deserialize, Serialize};

use super::OrderedMap;
use crate::types::{ContentBlock, JsonObject, Model, Usage, now_ms};

/// An image judged with the classifier state: `{"type":"image","data":...,"mimeType":...}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "image", rename_all = "camelCase")]
pub struct ImageContent {
    /// Base64 image data.
    pub data: String,
    pub mime_type: String,
}

impl From<ImageContent> for ContentBlock {
    fn from(image: ImageContent) -> Self {
        ContentBlock::Image { data: image.data, mime_type: image.mime_type }
    }
}

/// Meanings of the two answers of a bool question; empty strings are left out of prompts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BoolCriteria {
    #[serde(rename = "true")]
    pub when_true: String,
    #[serde(rename = "false")]
    pub when_false: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClassifierQuestion {
    /// Pick one key of `criteria`; values describe the keys.
    Choice {
        instructions: String,
        criteria: OrderedMap<String>,
    },
    /// Rate on the levels of `criteria`, lowest first.
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
    Bool {
        instructions: String,
        criteria: BoolCriteria,
    },
}

impl ClassifierQuestion {
    pub fn instructions(&self) -> &str {
        match self {
            Self::Choice { instructions, .. } | Self::Score { instructions, .. } | Self::Bool { instructions, .. } => {
                instructions
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifierContext {
    pub state: JsonObject,
    /// Images judged with `state`; only models whose `input` includes `image` accept them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageContent>>,
    pub questions: OrderedMap<ClassifierQuestion>,
}

impl ClassifierContext {
    pub fn has_images(&self) -> bool {
        self.images.as_ref().is_some_and(|images| !images.is_empty())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClassifierAnswer {
    Choice { choice: String, probabilities: OrderedMap<f64>, confidence: f64 },
    Score { score: f64, confidence: f64 },
    Bool { probability: f64 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClassifierStopReason {
    #[default]
    Stop,
    Error,
    Aborted,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifierResult {
    pub api: String,
    pub provider: String,
    pub model: String,
    pub answers: OrderedMap<ClassifierAnswer>,
    /// Token usage priced from the model catalog, when the service reports token counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub stop_reason: ClassifierStopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
}

impl ClassifierResult {
    /// An empty successful result for `model`, stamped now.
    pub fn new(model: &Model) -> Self {
        Self {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            answers: OrderedMap::new(),
            usage: None,
            stop_reason: ClassifierStopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        }
    }

    /// Mark the result failed: no answers, `aborted` or `error`, and the message.
    pub fn fail(&mut self, message: impl Into<String>, aborted: bool) {
        self.answers = OrderedMap::new();
        self.stop_reason = if aborted { ClassifierStopReason::Aborted } else { ClassifierStopReason::Error };
        self.error_message = Some(message.into());
    }
}
