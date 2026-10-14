//! The `Models` service over one conversation (upstream `services/models.ts` and `models-provider.ts`).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use pi_ai::simple_options::{clamp_thinking_level, supported_thinking_levels};
use pi_ai::{Model, ThinkingLevel};
use pi_chord::facets::{Facet, define_facet};
use pi_chord::services::{ServiceHandle, ServiceImpl, ServiceRef};
use pi_chord::state::{AttachedReplicatedState, MutableReplicatedState, StateListener};
use pi_chord::{BACKGROUND_CONTEXT, ChordError, ChordResult, Context, Json};
use pi_durable::harness::types::{AgentChange, AgentState, Change, ModelRef as DurableModelRef};
use pi_durable::harness::{AGENT_DOC, Conversation, Harness};
use serde::{Deserialize, Serialize};

use super::codec::{decode_arg, decode_result, encode};

pub static MODELS: LazyLock<ServiceRef> = LazyLock::new(|| ServiceRef::remote("pi.models"));

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSummary {
    pub provider: String,
    pub model_id: String,
    pub name: String,
    pub reasoning: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsCatalog {
    pub revision: u64,
    pub available_models: Vec<ModelSummary>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsConfiguration {
    pub model: Option<ModelRef>,
    pub thinking_level: ThinkingLevel,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum ModelsRefresh {
    Idle,
    Refreshing,
    Done,
    Warning { errors: BTreeMap<String, String> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelsState {
    pub catalog: ModelsCatalog,
    pub configuration: ModelsConfiguration,
    pub refresh: ModelsRefresh,
}

#[async_trait]
pub trait Models: Send + Sync {
    async fn cycle_thinking(&self, context: &Context) -> ChordResult<()>;
    async fn get_thinking_levels(&self, context: &Context) -> ChordResult<Vec<ThinkingLevel>>;
    async fn refresh(&self, context: &Context) -> ChordResult<()>;
    async fn select(&self, model: ModelRef, context: &Context) -> ChordResult<()>;
    async fn select_thinking(&self, level: ThinkingLevel, context: &Context) -> ChordResult<()>;
}

/// Calls `pi.models` through a service handle.
#[derive(Clone)]
pub struct ModelsClient(pub ServiceHandle);

impl ModelsClient {
    /// The replicated state, once hydrated.
    pub fn state(&self) -> ChordResult<Option<ModelsState>> {
        self.0.state("state")?.value()?.map(|value| decode_result(Some(value))).transpose()
    }
}

#[async_trait]
impl Models for ModelsClient {
    async fn cycle_thinking(&self, context: &Context) -> ChordResult<()> {
        self.0.call("cycleThinking", Vec::new(), context).await.map(drop)
    }
    async fn get_thinking_levels(&self, context: &Context) -> ChordResult<Vec<ThinkingLevel>> {
        decode_result(self.0.call("getThinkingLevels", Vec::new(), context).await?)
    }
    async fn refresh(&self, context: &Context) -> ChordResult<()> {
        self.0.call("refresh", Vec::new(), context).await.map(drop)
    }
    async fn select(&self, model: ModelRef, context: &Context) -> ChordResult<()> {
        self.0.call("select", vec![encode(&model)?], context).await.map(drop)
    }
    async fn select_thinking(&self, level: ThinkingLevel, context: &Context) -> ChordResult<()> {
        self.0.call("selectThinking", vec![encode(&level)?], context).await.map(drop)
    }
}

pub fn models_impl(service: Arc<dyn Models>, state: &MutableReplicatedState) -> ServiceImpl {
    let (cycle, levels, refresh, select, thinking) =
        (service.clone(), service.clone(), service.clone(), service.clone(), service);
    ServiceImpl::new()
        .state("state", state)
        .method("cycleThinking", move |_args: Vec<Json>, context| {
            let service = cycle.clone();
            async move { service.cycle_thinking(&context).await.map(|()| None) }
        })
        .method("getThinkingLevels", move |_args: Vec<Json>, context| {
            let service = levels.clone();
            async move { encode(&service.get_thinking_levels(&context).await?).map(Some) }
        })
        .method("refresh", move |_args: Vec<Json>, context| {
            let service = refresh.clone();
            async move { service.refresh(&context).await.map(|()| None) }
        })
        .method("select", move |args: Vec<Json>, context| {
            let service = select.clone();
            async move { service.select(decode_arg(&args, 0)?, &context).await.map(|()| None) }
        })
        .method("selectThinking", move |args: Vec<Json>, context| {
            let service = thinking.clone();
            async move { service.select_thinking(decode_arg(&args, 0)?, &context).await.map(|()| None) }
        })
}

/// Model lookup and refresh, as upstream's `ModelRuntime` provides them.
#[async_trait]
pub trait ModelCatalog: Send + Sync {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<Model>;
    fn available_snapshot(&self) -> Vec<Model>;
    /// Refresh providers; returns refresh errors by provider.
    async fn refresh(&self, context: &Context) -> BTreeMap<String, String>;
}

#[async_trait]
impl ModelCatalog for crate::agent_session::ModelRuntime {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<Model> {
        crate::agent_session::ModelRuntime::get_model(self, provider, model_id)
    }
    fn available_snapshot(&self) -> Vec<Model> {
        self.available_models()
    }
    async fn refresh(&self, _context: &Context) -> BTreeMap<String, String> {
        self.refresh_configured_providers().await;
        BTreeMap::new()
    }
}

/// Persists the default model selection, as upstream's `SettingsManager` does.
#[async_trait]
pub trait ModelDefaults: Send + Sync {
    async fn set_default_model_and_provider(&self, provider: &str, model_id: &str) -> ChordResult<()>;
}

struct ModelsRuntime {
    conversation: Conversation,
    agent: AttachedReplicatedState,
    catalog: Option<Arc<dyn ModelCatalog>>,
    defaults: Option<Arc<dyn ModelDefaults>>,
    state: MutableReplicatedState,
    revision: AtomicU64,
}

fn agent_state(agent: &AttachedReplicatedState) -> AgentState {
    agent.value().deserialize_into().unwrap_or_default()
}

fn configuration_of(state: &AgentState) -> ModelsConfiguration {
    let model = state
        .model
        .as_ref()
        .map(|model| ModelRef { provider: model.provider.clone(), model_id: model.model_id.clone() });
    ModelsConfiguration { model, thinking_level: state.thinking_level.unwrap_or_default() }
}

impl ModelsRuntime {
    fn selected_model(&self) -> Option<Model> {
        let model = agent_state(&self.agent).model?;
        self.catalog.as_ref()?.get_model(&model.provider, &model.model_id)
    }

    fn thinking_levels(&self) -> Vec<ThinkingLevel> {
        self.selected_model().map_or_else(|| vec![ThinkingLevel::Off], |model| supported_thinking_levels(&model))
    }

    fn read_catalog(&self) -> ModelsCatalog {
        let mut available = self.catalog.as_ref().map(|catalog| catalog.available_snapshot()).unwrap_or_default();
        if let Some(selected) = self.selected_model()
            && !available.iter().any(|model| model.provider == selected.provider && model.id == selected.id)
        {
            available.push(selected);
        }
        let revision = self.revision.fetch_add(1, Ordering::SeqCst) + 1;
        let available_models = available
            .into_iter()
            .map(|model| ModelSummary {
                provider: model.provider,
                model_id: model.id,
                name: model.name,
                reasoning: model.reasoning,
            })
            .collect();
        ModelsCatalog { revision, available_models }
    }

    fn change(&self, context: &Context, apply: impl FnOnce(&mut ModelsState)) -> ChordResult<()> {
        let mut value: ModelsState = self.state.value().deserialize_into()?;
        apply(&mut value);
        let value = encode(&value)?;
        self.state.change(context, move |draft| {
            *draft = value;
            Ok(())
        })
    }

    fn sync_configuration(&self, context: &Context) -> ChordResult<()> {
        let next = configuration_of(&agent_state(&self.agent));
        let current: ModelsState = self.state.value().deserialize_into()?;
        if current.configuration == next {
            return Ok(());
        }
        self.change(context, |state| state.configuration = next)
    }

    async fn configure(&self, change: AgentChange, context: &Context) -> ChordResult<()> {
        Ok(self.conversation.configure(change, context).await?)
    }
}

#[async_trait]
impl Models for ModelsRuntime {
    async fn cycle_thinking(&self, context: &Context) -> ChordResult<()> {
        let levels = self.thinking_levels();
        let current = agent_state(&self.agent).thinking_level.unwrap_or_default();
        let index = levels.iter().position(|level| *level == current).map_or(0, |index| index + 1);
        let next = levels.get(index % levels.len()).copied().unwrap_or_default();
        self.configure(AgentChange { thinking_level: Change::Set(next), ..AgentChange::default() }, context).await
    }
    async fn get_thinking_levels(&self, _context: &Context) -> ChordResult<Vec<ThinkingLevel>> {
        Ok(self.thinking_levels())
    }
    async fn refresh(&self, context: &Context) -> ChordResult<()> {
        self.change(context, |state| state.refresh = ModelsRefresh::Refreshing)?;
        let errors = match &self.catalog {
            Some(catalog) => catalog.refresh(context).await,
            None => BTreeMap::new(),
        };
        let catalog = self.read_catalog();
        self.change(context, |state| {
            state.catalog = catalog;
            state.refresh = if errors.is_empty() { ModelsRefresh::Done } else { ModelsRefresh::Warning { errors } };
        })
    }
    async fn select(&self, model: ModelRef, context: &Context) -> ChordResult<()> {
        let selected = self.catalog.as_ref().and_then(|catalog| catalog.get_model(&model.provider, &model.model_id));
        let Some(selected) = selected else {
            return Err(ChordError::error(format!("Unknown model: {}/{}", model.provider, model.model_id)));
        };
        let level = clamp_thinking_level(&selected, agent_state(&self.agent).thinking_level.unwrap_or_default());
        let model = DurableModelRef { provider: selected.provider.clone(), model_id: selected.id.clone() };
        let change =
            AgentChange { model: Change::Set(model), thinking_level: Change::Set(level), ..AgentChange::default() };
        self.configure(change, context).await?;
        if let Some(defaults) = &self.defaults {
            defaults.set_default_model_and_provider(&selected.provider, &selected.id).await?;
        }
        Ok(())
    }
    async fn select_thinking(&self, level: ThinkingLevel, context: &Context) -> ChordResult<()> {
        let levels = self.thinking_levels();
        if !levels.contains(&level) {
            let names: Vec<&str> = levels.iter().map(|level| level.as_str()).collect();
            let message =
                format!("Thinking level {} is unavailable; choose one of: {}", level.as_str(), names.join(", "));
            return Err(ChordError::error(message));
        }
        self.configure(AgentChange { thinking_level: Change::Set(level), ..AgentChange::default() }, context).await
    }
}

/// Take the conversation's agent document, then build the facet that owns it.
pub async fn create_models_service_facet(
    harness: &Harness,
    conversation: &Conversation,
    catalog: Option<Arc<dyn ModelCatalog>>,
    defaults: Option<Arc<dyn ModelDefaults>>,
    context: &Context,
) -> ChordResult<Arc<dyn Facet>> {
    let agent = harness.session().document_state(&AGENT_DOC, conversation.id, context).await?;
    let Some(agent) = agent else {
        return Err(ChordError::error(format!("Conversation {} has no agent document", conversation.id)));
    };
    let conversation = conversation.clone();
    Ok(define_facet("@pi/models", move |env| {
        let owned = agent.clone();
        env.own(move || owned.dispose())?;
        let initial = ModelsState {
            catalog: ModelsCatalog { revision: 0, available_models: Vec::new() },
            configuration: configuration_of(&agent_state(&agent)),
            refresh: ModelsRefresh::Idle,
        };
        let state = env.replicated_state(encode(&initial)?)?;
        let runtime = Arc::new(ModelsRuntime {
            conversation: conversation.clone(),
            agent: agent.clone(),
            catalog: catalog.clone(),
            defaults: defaults.clone(),
            state: state.clone(),
            revision: AtomicU64::new(0),
        });
        env.provide(&MODELS, models_impl(runtime.clone(), &state))?;
        let activation_env = env.clone();
        env.on_activate(move || {
            let (runtime, env) = (runtime.clone(), activation_env.clone());
            async move {
                let catalog = runtime.read_catalog();
                runtime.change(&BACKGROUND_CONTEXT, |state| {
                    state.catalog = catalog;
                    state.refresh = ModelsRefresh::Idle;
                })?;
                let listener = runtime.clone();
                let sync = StateListener::sync(move |_value, context, _delivery| listener.sync_configuration(context));
                let subscription = runtime.agent.subscribe(sync);
                env.own(move || subscription.close())
            }
        })
    }))
}
