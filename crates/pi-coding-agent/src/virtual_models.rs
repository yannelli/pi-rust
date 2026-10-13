//! Virtual models (upstream `core/virtual-models.ts`, `docs/virtual-models.md`): catalog entries
//! that route each request to a physical model. The selection (`model_change`, the agent state
//! model) may name a virtual model; providers and assistant messages only see physical models.

use std::sync::Arc;

use async_trait::async_trait;
use pi_ai::{AssistantMessage, Message, Model, ModelCost, StopReason, ThinkingLevel};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::session::{Entry, EntryData};

/// API id of virtual catalog entries. Requests for it fail unless routed first.
pub const VIRTUAL_MODEL_API: &str = "pi-virtual";

/// Custom entry type that stores router state on the session branch.
pub const VIRTUAL_MODEL_STATE_ENTRY: &str = "pi.virtual-model-state";

/// Why a request is routed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelRouteReason {
    /// First request after a message the user wrote (prompt, steering, or follow-up).
    User,
    /// Any other request of the agent loop, such as after tool results or extension messages.
    Continuation,
    /// Automatic retry after a failed request, including after compaction for an overflow.
    Retry,
    /// A request outside the agent loop, such as a compaction summary.
    Direct,
}

impl ModelRouteReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Continuation => "continuation",
            Self::Retry => "retry",
            Self::Direct => "direct",
        }
    }
}

/// A physical model and the thinking level a response used.
#[derive(Clone, Debug, PartialEq)]
pub struct RoutedModel {
    pub model: Model,
    pub thinking_level: Option<ThinkingLevel>,
}

/// The failed request a retry follows; `messages` of the retry no longer contain it.
#[derive(Clone, Debug, PartialEq)]
pub struct FailedRequest {
    pub model: Model,
    pub thinking_level: Option<ThinkingLevel>,
    /// Carries `stop_reason` and `error_message`.
    pub message: AssistantMessage,
}

/// Input of [`ModelRouter::route`] (upstream `ModelRouteRequest`).
#[derive(Clone, Debug)]
pub struct ModelRouteRequest {
    /// The selected virtual model.
    pub model: Model,
    /// The selected thinking level; its meaning is up to the router.
    pub thinking_level: ThinkingLevel,
    pub reason: ModelRouteReason,
    /// Physical model and thinking level of the latest successful response in `messages`.
    pub previous: Option<RoutedModel>,
    /// For `retry`: the failed request. Absent when routing itself failed.
    pub failed: Option<FailedRequest>,
    /// Router state last returned on this session branch; `None` before the first and for `direct`.
    pub state: Option<Value>,
    /// The conversation for this request, including system messages.
    pub messages: Vec<Message>,
    pub cancel: CancellationToken,
}

impl ModelRouteRequest {
    /// The request in the upstream JSON shape, for routers outside Rust.
    pub fn to_json(&self) -> Value {
        let routed = |model: &Model, level: Option<ThinkingLevel>| {
            let mut value = json!({"model": model});
            if let Some(level) = level {
                value["thinkingLevel"] = json!(level.as_str());
            }
            value
        };
        let mut object = Map::new();
        object.insert("model".into(), json!(self.model));
        object.insert("thinkingLevel".into(), json!(self.thinking_level.as_str()));
        object.insert("reason".into(), json!(self.reason.as_str()));
        if let Some(previous) = &self.previous {
            object.insert("previous".into(), routed(&previous.model, previous.thinking_level));
        }
        if let Some(failed) = &self.failed {
            let mut value = routed(&failed.model, failed.thinking_level);
            value["message"] = json!(Message::Assistant(failed.message.clone()));
            object.insert("failed".into(), value);
        }
        if let Some(state) = &self.state {
            object.insert("state".into(), state.clone());
        }
        object.insert("messages".into(), json!(self.messages));
        Value::Object(object)
    }
}

/// The physical model and thinking level for one request (upstream `ModelRoute`).
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRoute {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    /// New router state, stored on the session branch even when it equals the current one.
    /// `None` keeps the current state. Ignored for `direct` requests.
    pub state: Option<Value>,
}

/// Physical models a router can pick from.
pub trait ModelLookup: Send + Sync {
    /// A physical chat model of the catalog.
    fn find(&self, provider: &str, id: &str) -> Option<Model>;
    fn has_configured_auth(&self, provider: &str) -> bool;
}

/// Picks the physical model of each request (upstream `VirtualModelDefinition.route`). The
/// returned model must be physical and have credentials; its thinking level is clamped.
#[async_trait]
pub trait ModelRouter: Send + Sync {
    async fn route(&self, request: ModelRouteRequest, models: &dyn ModelLookup) -> anyhow::Result<ModelRoute>;
}

#[async_trait]
impl<F> ModelRouter for F
where
    F: Fn(&ModelRouteRequest, &dyn ModelLookup) -> anyhow::Result<ModelRoute> + Send + Sync,
{
    async fn route(&self, request: ModelRouteRequest, models: &dyn ModelLookup) -> anyhow::Result<ModelRoute> {
        self(&request, models)
    }
}

/// A virtual model to register (upstream `VirtualModelDefinition`).
#[derive(Clone)]
pub struct VirtualModelDefinition {
    /// Provider the model is listed under; it may also list physical models.
    pub provider: String,
    /// Must not be the id of a physical model of `provider`.
    pub id: String,
    pub name: String,
    /// Levels offered for selection; `["off"]` when `None`.
    pub thinking_levels: Option<Vec<ThinkingLevel>>,
    /// Limits shown before the first response; unknown (0) when unset.
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    /// Input types offered for selection; text and images when `None`.
    pub input: Option<Vec<String>>,
    pub router: Arc<dyn ModelRouter>,
}

impl VirtualModelDefinition {
    pub fn new(provider: &str, id: &str, name: &str, router: Arc<dyn ModelRouter>) -> Self {
        Self {
            provider: provider.into(),
            id: id.into(),
            name: name.into(),
            thinking_levels: None,
            context_window: None,
            max_tokens: None,
            input: None,
            router,
        }
    }
}

impl std::fmt::Debug for VirtualModelDefinition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VirtualModelDefinition")
            .field("provider", &self.provider)
            .field("id", &self.id)
            .finish()
    }
}

/// Whether a model names a virtual model.
pub fn is_virtual_model(model: &Model) -> bool {
    model.api == VIRTUAL_MODEL_API
}

/// Whether an assistant message names a virtual model, which only failed routing leaves.
pub fn is_virtual_message(message: &AssistantMessage) -> bool {
    message.api == VIRTUAL_MODEL_API
}

/// The latest successful response; failed and aborted requests, including failed routing, are
/// skipped, so its model is physical.
pub fn find_latest_response(messages: &[Message]) -> Option<&AssistantMessage> {
    messages.iter().rev().find_map(|message| match message {
        Message::Assistant(assistant) if !matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) => {
            Some(assistant)
        }
        _ => None,
    })
}

/// The model selection a session branch records. A virtual `model_change` holds until the next
/// `model_change`; otherwise the latest physical response wins. A virtual model that is no
/// longer registered does not hold, so the selection falls back to the model that answered last.
pub fn get_branch_selection(
    branch: &[&Entry],
    get_model: &dyn Fn(&str, &str) -> Option<Model>,
) -> Option<(String, String)> {
    for (index, entry) in branch.iter().enumerate().rev() {
        match entry.data() {
            EntryData::ModelChange { provider, model_id } => return Some((provider.clone(), model_id.clone())),
            EntryData::Message(Message::Assistant(message)) if !is_virtual_message(message) => {
                let response = (message.provider.clone(), message.model.clone());
                let change = branch[..index].iter().rev().find_map(|entry| match entry.data() {
                    EntryData::ModelChange { provider, model_id } => Some((provider.clone(), model_id.clone())),
                    _ => None,
                });
                let holds = change
                    .as_ref()
                    .and_then(|(provider, id)| get_model(provider, id))
                    .is_some_and(|model| is_virtual_model(&model));
                return Some(if holds { change.unwrap_or(response) } else { response });
            }
            _ => {}
        }
    }
    None
}

/// The latest router state a session branch stores for a virtual model.
pub fn get_virtual_model_state(branch: &[&Entry], provider: &str, model_id: &str) -> Option<Value> {
    branch.iter().rev().find_map(|entry| match entry.data() {
        EntryData::Custom { custom_type, data: Some(data) } if custom_type == VIRTUAL_MODEL_STATE_ENTRY => {
            let matches = data.get("provider").and_then(Value::as_str) == Some(provider)
                && data.get("modelId").and_then(Value::as_str) == Some(model_id);
            matches.then(|| data.get("state").cloned().unwrap_or(Value::Null))
        }
        _ => None,
    })
}

/// Data of a `pi.virtual-model-state` custom entry.
pub fn virtual_model_state_data(provider: &str, model_id: &str, state: &Value) -> Value {
    json!({"provider": provider, "modelId": model_id, "state": state})
}

/// The catalog entry of a virtual model.
pub fn create_virtual_model(definition: &VirtualModelDefinition) -> Model {
    let levels = definition.thinking_levels.clone().unwrap_or_else(|| vec![ThinkingLevel::Off]);
    let map = ThinkingLevel::ALL
        .iter()
        .map(|level| (level.as_str().to_string(), levels.contains(level).then(|| level.as_str().to_string())))
        .collect();
    Model {
        id: definition.id.clone(),
        name: definition.name.clone(),
        api: VIRTUAL_MODEL_API.into(),
        provider: definition.provider.clone(),
        base_url: String::new(),
        reasoning: levels.iter().any(|level| *level != ThinkingLevel::Off),
        thinking_level_map: Some(map),
        input: definition.input.clone().unwrap_or_else(|| vec!["text".into(), "image".into()]),
        cost: ModelCost::default(),
        context_window: definition.context_window.unwrap_or(0),
        max_tokens: definition.max_tokens.unwrap_or(0),
        ..Model::default()
    }
}

#[cfg(test)]
#[path = "virtual_models_tests.rs"]
mod tests;
