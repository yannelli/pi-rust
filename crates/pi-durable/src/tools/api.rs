//! What the coding tools use of a call's `ToolExecutionApi`, as a trait so a test or host can run a tool without a
//! Harness, as upstream tests do with a partial api object.

use async_trait::async_trait;
use pi_ai::Model;
use pi_chord::Context;

use crate::env::{SharedEnv, ShellOutputSkip, ShellOutputWindow};
use crate::errors::DurableResult;
use crate::harness::{ToolDiagnostic, ToolExecutionApi};
use crate::images::{DEFAULT_IMAGE_LIMITS, ImageLimits};

/// What the conversation's model takes: whether images, and within which limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageModel {
    pub vision: bool,
    pub limits: ImageLimits,
}

impl Default for ImageModel {
    fn default() -> Self {
        image_model_of(None)
    }
}

/// `DEFAULT_IMAGE_LIMITS` overridden by the model's `inputLimits.images.resize`. Without a model, it counts as one
/// that takes images within the defaults.
pub fn image_model_of(model: Option<&Model>) -> ImageModel {
    let resize = model.and_then(|model| model.input_limits.as_ref()?.images?.resize);
    ImageModel {
        vision: model.is_none_or(|model| model.input.iter().any(|input| input == "image")),
        limits: ImageLimits {
            max_width: resize.and_then(|r| r.max_width).unwrap_or(DEFAULT_IMAGE_LIMITS.max_width),
            max_height: resize.and_then(|r| r.max_height).unwrap_or(DEFAULT_IMAGE_LIMITS.max_height),
            max_bytes: resize.and_then(|r| r.max_bytes).map_or(DEFAULT_IMAGE_LIMITS.max_bytes, |b| b as usize),
        },
    }
}

/// The operations of one tool call that the coding tools need.
#[async_trait]
pub trait ToolCallApi: Send + Sync {
    fn env(&self) -> Option<SharedEnv>;
    /// The tail this call's output keeps; `None` for head retention.
    fn output_window(&self) -> Option<ShellOutputWindow>;
    /// Append running output; `skipped` counts output the environment omitted before it.
    fn output(&self, chunk: &str, skipped: Option<ShellOutputSkip>) -> DurableResult<()>;
    /// The output kept so far and whether any was dropped.
    fn retained_output(&self) -> DurableResult<(String, bool)>;
    fn diagnostic(&self, diagnostic: ToolDiagnostic) -> DurableResult<()>;
    /// The conversation model's image input.
    async fn image_model(&self, context: &Context) -> DurableResult<ImageModel>;
    /// The Harness api behind this call, when there is one.
    fn execution_api(&self) -> Option<&ToolExecutionApi> {
        None
    }
}

#[async_trait]
impl ToolCallApi for ToolExecutionApi {
    fn env(&self) -> Option<SharedEnv> {
        self.env.clone()
    }

    fn output_window(&self) -> Option<ShellOutputWindow> {
        self.output_window
    }

    fn output(&self, chunk: &str, skipped: Option<ShellOutputSkip>) -> DurableResult<()> {
        match skipped {
            None => ToolExecutionApi::output(self, chunk),
            Some(skip) => self.output_bytes(chunk.as_bytes(), Some(skip)),
        }
    }

    fn retained_output(&self) -> DurableResult<(String, bool)> {
        ToolExecutionApi::retained_output(self)
    }

    fn diagnostic(&self, diagnostic: ToolDiagnostic) -> DurableResult<()> {
        ToolExecutionApi::diagnostic(self, diagnostic)
    }

    async fn image_model(&self, context: &Context) -> DurableResult<ImageModel> {
        let model = self.agent(context).await?.model;
        let model = model.and_then(|model| self.models().get_model(&model.provider, &model.model_id));
        Ok(image_model_of(model.as_ref()))
    }

    fn execution_api(&self) -> Option<&ToolExecutionApi> {
        Some(self)
    }
}
