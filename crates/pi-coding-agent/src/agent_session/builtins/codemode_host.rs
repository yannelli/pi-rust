//! The session as the `codemode` tool sees it (upstream `extensions/codemode/execute.ts` over
//! `ExtensionContext`): the callable tools, nested calls outside the agent loop, the store on the
//! current branch, and the model registry with the session's credentials.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use pi_agent::{AgentContext, RunToolCallOptions, run_tool_call};
use pi_ai::classify::{ClassifierContext, ClassifierOptions};
use pi_ai::images::{ImagesContext, ImagesOptions};
use pi_ai::model_operations::{classifier_error_result, image_error_result, model_type};
use pi_ai::{AssistantMessage, Model, RequestOptions, ToolCall};
use pi_codemode::{
    CODEMODE_STORE_ENTRY_TYPE, CodemodeHost, CodemodeModels, CodemodeStoreEntryData, CodemodeToolInfo, ModelType,
    NestedToolOutcome, read_codemode_store,
};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::agent_session::AgentSession;
use crate::agent_session::auth::request_auth;
use crate::agent_session::model_runtime::ModelRuntime;
use crate::session::EntryData;

pub(crate) struct SessionCodemodeHost {
    session: Weak<AgentSession>,
    models: SessionModels,
}

impl SessionCodemodeHost {
    pub fn new(session: Weak<AgentSession>, runtime: Arc<ModelRuntime>) -> Self {
        Self { session, models: SessionModels { runtime } }
    }
}

fn error_outcome(id: String, text: String) -> NestedToolOutcome {
    NestedToolOutcome {
        tool_call_id: id,
        content: vec![pi_ai::ContentBlock::text(text)],
        is_error: true,
        ..Default::default()
    }
}

#[async_trait]
impl CodemodeHost for SessionCodemodeHost {
    fn tools(&self) -> Vec<CodemodeToolInfo> {
        let Some(session) = self.session.upgrade() else { return Vec::new() };
        let callable = session.with_tool_registry(|registry| registry.callable_tools());
        callable.iter().map(|tool| CodemodeToolInfo::from_tool(tool.as_ref())).collect()
    }

    /// Calls outside the agent loop run against the callable tools without the loop's hooks.
    async fn execute_tool(
        &self,
        parent: &str,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> NestedToolOutcome {
        let id = format!("{parent}/0");
        let Some(session) = self.session.upgrade() else {
            return error_outcome(id, "The session has ended".into());
        };
        let tools = session.with_tool_registry(|registry| registry.callable_tools());
        let arguments = args.as_object().cloned().unwrap_or_default();
        let call = ToolCall { id: id.clone(), name: name.into(), arguments, ..Default::default() };
        let assistant = AssistantMessage::default();
        let context = AgentContext::default();
        let options = RunToolCallOptions {
            tools: &tools,
            assistant_message: &assistant,
            context: &context,
            before_tool_call: None,
            after_tool_call: None,
            parent_tool_call_id: Some(parent),
            tool_context: None,
            cancel,
            on_update: None,
        };
        let outcome = run_tool_call(call, options).await;
        NestedToolOutcome {
            tool_call_id: outcome.tool_call.id,
            content: outcome.result.content,
            structured_content: outcome.result.structured_content,
            is_error: outcome.is_error,
        }
    }

    fn store_snapshot(&self) -> Map<String, Value> {
        let Some(session) = self.session.upgrade() else { return Map::new() };
        let manager = session.session_manager();
        let branch = manager.get_branch(None);
        let entries = branch.iter().filter_map(|entry| match entry.data() {
            EntryData::Custom { custom_type, data } => Some((custom_type, data)),
            _ => None,
        });
        let entries: Vec<(String, Value)> =
            entries.map(|(kind, data)| (kind.clone(), data.clone().unwrap_or(Value::Null))).collect();
        read_codemode_store(entries.iter().map(|(kind, data)| (kind.as_str(), data)))
    }

    fn append_store_entry(&self, data: CodemodeStoreEntryData) {
        let Some(session) = self.session.upgrade() else { return };
        let Ok(value) = serde_json::to_value(&data) else { return };
        session.append_custom_entry(CODEMODE_STORE_ENTRY_TYPE, Some(value));
    }

    fn models(&self) -> Option<&dyn CodemodeModels> {
        Some(&self.models)
    }
}

/// The model registry for `models.*`: catalog lookups and classifier and image calls with the
/// credentials of the catalog entry, never with fields the script supplied.
pub(crate) struct SessionModels {
    runtime: Arc<ModelRuntime>,
}

fn matches(model: &Model, kind: ModelType, provider: Option<&str>) -> bool {
    model_type(model) == kind.as_str() && provider.is_none_or(|provider| model.provider == provider)
}

fn to_value(model: &Model) -> Value {
    serde_json::to_value(model).unwrap_or(Value::Null)
}

impl SessionModels {
    fn find(&self, kind: ModelType, model: &Value) -> Option<Model> {
        let field = |key: &str| model.get(key).and_then(Value::as_str).map(str::to_string);
        let (provider, id) = (field("provider")?, field("id")?);
        self.runtime
            .registry()
            .all_models()
            .iter()
            .find(|m| m.provider == provider && m.id == id && matches(m, kind, None))
            .cloned()
    }

    async fn request(&self, model: &Model, cancel: CancellationToken) -> Result<(Model, RequestOptions), String> {
        let auth = request_auth(&self.runtime, model).await?;
        let mut request = RequestOptions { api_key: auth.auth.api_key.clone(), cancel, ..RequestOptions::default() };
        request.headers.extend(auth.auth.headers.clone());
        if let Some(env) = &auth.env {
            request.env.extend(env.clone());
        }
        Ok((auth.model, request))
    }
}

#[async_trait]
impl CodemodeModels for SessionModels {
    fn get_models_of_type(&self, kind: ModelType, provider: Option<&str>) -> Vec<Value> {
        let registry = self.runtime.registry();
        registry.all_models().iter().filter(|model| matches(model, kind, provider)).map(to_value).collect()
    }

    async fn get_available_of_type(
        &self,
        kind: ModelType,
        provider: Option<&str>,
        _cancel: CancellationToken,
    ) -> anyhow::Result<Vec<Value>> {
        let models: Vec<Model> = {
            let registry = self.runtime.registry();
            registry.all_models().iter().filter(|model| matches(model, kind, provider)).cloned().collect()
        };
        let mut available = Vec::new();
        let mut checked: Vec<(String, bool)> = Vec::new();
        for model in models {
            let usable = match checked.iter().find(|(provider, _)| *provider == model.provider) {
                Some((_, usable)) => *usable,
                None => {
                    let usable = self.runtime.has_configured_auth(&model.provider)
                        || self.runtime.check_auth(&model.provider).await;
                    checked.push((model.provider.clone(), usable));
                    usable
                }
            };
            if usable {
                available.push(to_value(&model));
            }
        }
        Ok(available)
    }

    fn get_model_of_type(&self, kind: ModelType, provider: &str, id: &str) -> Option<Value> {
        let registry = self.runtime.registry();
        let found = registry.all_models().iter().find(|model| model.provider == provider && model.id == id);
        found.filter(|model| matches(model, kind, None)).map(to_value)
    }

    async fn classify(&self, model: &Value, context: Value, cancel: CancellationToken) -> anyhow::Result<Value> {
        let model =
            self.find(ModelType::Classifier, model).ok_or_else(|| anyhow::anyhow!("Unknown classifier model"))?;
        let context: ClassifierContext = serde_json::from_value(context)?;
        let aborted = cancel.clone();
        let result = match self.request(&model, cancel).await {
            Ok((resolved, request)) => {
                let options = ClassifierOptions { request, temperature: None };
                self.runtime.classify(&resolved, &context, options).await
            }
            Err(message) => classifier_error_result(&model, message, aborted.is_cancelled()),
        };
        Ok(serde_json::to_value(result)?)
    }

    async fn generate_images(&self, model: &Value, context: Value, cancel: CancellationToken) -> anyhow::Result<Value> {
        let model = self.find(ModelType::Image, model).ok_or_else(|| anyhow::anyhow!("Unknown image model"))?;
        let context: ImagesContext = serde_json::from_value(context)?;
        let aborted = cancel.clone();
        let result = match self.request(&model, cancel).await {
            Ok((resolved, request)) => {
                let options = ImagesOptions { request, metadata: None };
                self.runtime.generate_images(&resolved, &context, options).await
            }
            Err(message) => image_error_result(&model, message, aborted.is_cancelled()),
        };
        Ok(serde_json::to_value(result)?)
    }
}
