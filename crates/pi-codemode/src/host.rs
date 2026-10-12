//! What the application provides to the `codemode` tool: the callable tools, nested tool
//! execution, the store on the session branch, and the model catalog.

use async_trait::async_trait;
use pi_ai::ContentBlock;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::store::CodemodeStoreEntryData;

/// How a tool is offered: declared to the model, only to the model, only to scripts, found by
/// search, or not at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolExposure {
    #[default]
    Direct,
    ModelOnly,
    Codemode,
    Deferred,
    Hidden,
}

/// A group of related tools, such as the tools of one MCP server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolNamespace {
    /// For example `mcp__docs`.
    pub name: String,
    /// Short summary shown once with the group in tool listings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Longer usage guidance, such as MCP server instructions. Only `describeNamespace()` returns it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl ToolNamespace {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), ..Self::default() }
    }
}

/// A tool as scripts and listings see it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodemodeToolInfo {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: Value,
    /// Schema of the structured result. Tools without one resolve to their text output.
    pub output_schema: Option<Value>,
    pub namespace: Option<ToolNamespace>,
    /// The tool's prompt guidelines, which listings show after its description.
    pub prompt_guidelines: Vec<String>,
}

impl CodemodeToolInfo {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self { name: name.into(), description: description.into(), parameters, ..Self::default() }
    }
}

/// The result of a nested tool call, as the session's tool pipeline reports it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NestedToolOutcome {
    /// Id of the nested call, `<codemode call id>/<n>`.
    pub tool_call_id: String,
    pub content: Vec<ContentBlock>,
    pub structured_content: Option<Value>,
    pub is_error: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelType {
    Chat,
    Image,
    Classifier,
}

impl ModelType {
    pub const ALL: [ModelType; 3] = [ModelType::Chat, ModelType::Image, ModelType::Classifier];

    pub fn as_str(self) -> &'static str {
        match self {
            ModelType::Chat => "chat",
            ModelType::Image => "image",
            ModelType::Classifier => "classifier",
        }
    }
}

/// The model registry as scripts reach it through `models.*`. Models are catalog entries as JSON
/// objects with at least `provider` and `id`. Results use the upstream `ClassifierResult` and
/// `ImagesResult` shapes: `stopReason`, optional `errorMessage` and `usage`, and image `output`.
#[async_trait]
pub trait CodemodeModels: Send + Sync {
    /// Every known model of a type, optionally for one provider.
    fn get_models_of_type(&self, model_type: ModelType, provider: Option<&str>) -> Vec<Value>;
    /// Models of a type whose provider has working credentials.
    async fn get_available_of_type(
        &self,
        model_type: ModelType,
        provider: Option<&str>,
        cancel: CancellationToken,
    ) -> anyhow::Result<Vec<Value>>;
    /// One catalog entry.
    fn get_model_of_type(&self, model_type: ModelType, provider: &str, id: &str) -> Option<Value>;
    /// Run a classifier with the session's credentials. Provider errors come back as results.
    async fn classify(&self, model: &Value, context: Value, cancel: CancellationToken) -> anyhow::Result<Value>;
    /// Run an image model with the session's credentials. Provider errors come back as results.
    async fn generate_images(&self, model: &Value, context: Value, cancel: CancellationToken) -> anyhow::Result<Value>;
}

/// The session as the `codemode` tool needs it. The application implements it.
#[async_trait]
pub trait CodemodeHost: Send + Sync {
    /// Tools scripts may call. A tool named `codemode` is left out.
    fn tools(&self) -> Vec<CodemodeToolInfo>;

    /// Run a tool through the session's tool pipeline (validation, hooks, permission checks).
    /// Failures, including unknown tools and blocked calls, come back with `is_error`.
    async fn execute_tool(
        &self,
        parent_tool_call_id: &str,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> NestedToolOutcome;

    /// Values `load()` reads: the `codemode-store` entries on the current branch, folded with
    /// [`read_codemode_store`](crate::store::read_codemode_store).
    fn store_snapshot(&self) -> Map<String, Value>;

    /// Append a successful script's writes to the session as a `codemode-store` custom entry.
    fn append_store_entry(&self, data: CodemodeStoreEntryData);

    /// The model registry for `models.*`. Without one, scripts get no `models` namespace.
    fn models(&self) -> Option<&dyn CodemodeModels> {
        None
    }
}
