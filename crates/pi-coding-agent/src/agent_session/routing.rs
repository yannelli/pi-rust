//! Virtual model routing in the session (upstream `_installAgentRequestProjection`,
//! `_recordSelection`, `_modelForMessage`, `_limitsModel`, and `routedModel`): each request of a
//! virtual selection asks the router for a physical model, and router state follows the branch.

use std::sync::{Arc, Weak};

use pi_agent::{AgentRequestUpdate, StreamFn};
use pi_ai::{Message, Model, StopReason, ThinkingLevel, error_stream};
use tokio_util::sync::CancellationToken;

use super::runtime_virtual::RouteOptions;
use super::{AgentSession, AgentSessionEvent, CompactionReason, lock};
use crate::compaction::exceeds_compaction_threshold;
use crate::virtual_models::{
    ModelRouteReason, RoutedModel, VIRTUAL_MODEL_STATE_ENTRY, VirtualModelDefinition, find_latest_response,
    get_branch_selection, get_virtual_model_state, is_virtual_model, virtual_model_state_data,
};

impl AgentSession {
    /// Register a virtual model with the session's model runtime (upstream
    /// `modelRuntime.registerVirtualModel`). It is listed like any other model.
    pub fn register_virtual_model(&self, definition: VirtualModelDefinition) -> anyhow::Result<()> {
        self.model_runtime().register_virtual_model(definition)
    }

    /// Remove a virtual model; false when it was not registered.
    pub fn unregister_virtual_model(&self, provider: &str, id: &str) -> bool {
        self.model_runtime().unregister_virtual_model(provider, id)
    }

    /// Under a virtual selection, the physical model and thinking level of the latest successful
    /// response (the footer shows it next to the selection, `/session` lists its cost).
    pub fn routed_model(&self) -> Option<RoutedModel> {
        if !self.model().is_some_and(|model| is_virtual_model(&model)) {
            return None;
        }
        let latest = self.with_state(|state| find_latest_response(&state.messages).cloned())?;
        let model = self.model_runtime().physical_model(&latest.provider, &latest.model)?;
        Some(RoutedModel { model, thinking_level: latest.thinking_level })
    }

    /// The model whose limits apply to the conversation: the routed model, else the selection.
    pub fn limits_model(&self) -> Option<Model> {
        self.routed_model().map(|routed| routed.model).or_else(|| self.model())
    }

    /// Record the selection on the branch when the branch implies another one, so a resume
    /// restores it.
    pub(crate) fn record_selection(&self) {
        self.record_model_selection();
        self.record_thinking_level();
    }

    /// Responses cannot record a virtual selection; they name physical models.
    fn record_model_selection(&self) {
        let Some(model) = self.model() else { return };
        let runtime = self.model_runtime().clone();
        let get_model = |provider: &str, id: &str| runtime.get_model(provider, id);
        let recorded = {
            let session = self.session_manager();
            get_branch_selection(&session.get_branch(None), &get_model)
        };
        let Some((provider, id)) = recorded else { return };
        if provider == model.provider && id == model.id {
            return;
        }
        let recorded_virtual = runtime.get_model(&provider, &id).is_some_and(|model| is_virtual_model(&model));
        if !is_virtual_model(&model) && !recorded_virtual {
            return;
        }
        if let Err(error) = self.session_manager().append_model_change(&model.provider, &model.id) {
            eprintln!("Warning: could not save session: {error:#}");
        }
    }

    /// A `/tree` move can leave the latest thinking level change on an abandoned branch (upstream
    /// issue 11451). The branch level is clamped to the model first, so a live level that is only
    /// its clamped value does not replace it.
    fn record_thinking_level(&self) {
        let live = self.thinking_level();
        let recorded = {
            let session = self.session_manager();
            crate::session::context::context_settings(&session.get_branch(None)).1
        };
        let Some(recorded) = recorded else { return };
        if self.clamp_thinking_level(recorded) == live {
            return;
        }
        if let Err(error) = self.session_manager().append_thinking_level_change(live) {
            eprintln!("Warning: could not save session: {error:#}");
        }
    }

    /// The request context and, under a virtual selection, the routed model. A routing failure
    /// keeps the virtual model, whose request then ends with the error response.
    pub(crate) async fn prepare_request_update(&self, cancel: CancellationToken) -> AgentRequestUpdate {
        let failed = lock(&self.state).failed_response.take();
        let mut context = self.projected_context();
        let (model, thinking_level) = self.with_state(|state| (state.model.clone(), state.thinking_level));
        if !is_virtual_model(&model) {
            return AgentRequestUpdate { context: Some(context), ..AgentRequestUpdate::default() };
        }
        // Only messages the user wrote start a turn; extension messages can follow them.
        let start = context.messages.iter().rposition(|message| matches!(message, Message::Assistant(_)));
        let user_turn = context.messages[start.map_or(0, |index| index + 1)..]
            .iter()
            .any(|message| matches!(message, Message::User(_)));
        let reason = match (&failed, user_turn) {
            (Some(_), _) => ModelRouteReason::Retry,
            (None, true) => ModelRouteReason::User,
            (None, false) => ModelRouteReason::Continuation,
        };
        let state = {
            let session = self.session_manager();
            get_virtual_model_state(&session.get_branch(None), &model.provider, &model.id)
        };
        let messages = crate::messages::convert_to_llm(&context.messages);
        let options = RouteOptions { reason, thinking_level, cancel, failed, state };
        let route = match self.model_runtime().resolve_model(&model, messages, options).await {
            Ok(route) => route,
            Err(error) => {
                lock(&self.state).route_failure = Some(format!("{error:#}"));
                return AgentRequestUpdate { context: Some(context), ..AgentRequestUpdate::default() };
            }
        };
        if let Some(state) = &route.state {
            let data = virtual_model_state_data(&model.provider, &model.id, state);
            let appended = self.session_manager().append_custom_entry(VIRTUAL_MODEL_STATE_ENTRY, Some(data));
            match appended {
                Ok(id) => self.emit_entry(&id),
                Err(error) => eprintln!("Warning: could not save session: {error:#}"),
            }
        }
        let routed = RoutedModel { model: route.model.clone(), thinking_level: Some(route.thinking_level) };
        self.emit(&AgentSessionEvent::ModelRouted { route: &routed, reason });
        // The route stands; a smaller window of the routed model compacts before the request.
        let settings = self.compaction_settings(Some(&model));
        let exceeds = exceeds_compaction_threshold(&self.session_manager(), &route.model, &settings);
        if exceeds {
            self.run_auto_compaction(CompactionReason::Threshold, false).await;
            context = self.projected_context();
        }
        AgentRequestUpdate {
            context: Some(context),
            model: Some(route.model),
            thinking_level: Some(route.thinking_level),
        }
    }

    /// The model and thinking level of a summary: a virtual selection is routed (`direct`)
    /// first, so summaries size their input and output from the model they get.
    pub(crate) async fn summary_model(
        &self,
        model: &Model,
        cancel: &CancellationToken,
    ) -> anyhow::Result<(Model, ThinkingLevel)> {
        if !is_virtual_model(model) {
            return Ok((model.clone(), self.thinking_level()));
        }
        let mut options = RouteOptions::new(ModelRouteReason::Direct, self.thinking_level());
        options.cancel = cancel.clone();
        let messages = crate::messages::convert_to_llm(&self.messages());
        let route = self.model_runtime().resolve_model(model, messages, options).await?;
        let routed = RoutedModel { model: route.model.clone(), thinking_level: Some(route.thinking_level) };
        self.emit(&AgentSessionEvent::ModelRouted { route: &routed, reason: ModelRouteReason::Direct });
        Ok((route.model, route.thinking_level))
    }

    /// The agent loop's stream function: requests whose routing failed end with that error, and
    /// session requests start cache warming before they are sent.
    pub(crate) fn loop_stream_fn(weak: Weak<AgentSession>) -> StreamFn {
        Arc::new(move |model: &Model, context: &pi_ai::Context, options: pi_ai::StreamOptions| {
            let Some(session) = weak.upgrade() else {
                return error_stream(model, StopReason::Aborted, "Session disposed");
            };
            if is_virtual_model(model) {
                let failure = lock(&session.state).route_failure.take();
                let message = failure.unwrap_or_else(|| {
                    format!("Virtual model {}/{} must be routed before streaming", model.provider, model.id)
                });
                return error_stream(model, StopReason::Error, message);
            }
            session.start_cache_warming(model, context, &options);
            (session.stream_fn())(model, context, options)
        })
    }
}
