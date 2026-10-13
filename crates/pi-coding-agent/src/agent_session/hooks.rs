//! Agent hooks of a session (upstream `_installAgentRequestProjection`,
//! `_installAgentNextTurnRefresh`, and `_afterToolCall`): every request uses the session
//! projection, turns compact and declare prompt changes in between, and tool images are resized.

use std::sync::{Arc, Weak};

use futures::FutureExt;
use pi_agent::{
    AfterToolCallFn, AfterToolCallResult, AgentContext, AgentHooks, AgentLoopTurnUpdate, AgentRequestUpdate,
    ConvertToLlmFn, PrepareNextTurnFn, PrepareRequestFn,
};
use pi_ai::Message;

use super::AgentSession;
use crate::images::{ProcessImageOptions, block_images, normalize_tool_result_images};

impl AgentSession {
    /// The request context: the session projection with the active tools.
    fn projected_context(&self) -> AgentContext {
        let messages = self.session_manager().build_session_context().messages;
        let tools = self.with_state(|state| state.tools.clone());
        AgentContext { system_prompt: None, messages, tools }
    }

    /// Between turns of one run: compact when the threshold is crossed, then declare prompt
    /// and tool changes made during the run in a system message before the next request.
    async fn prepare_next_turn(&self) -> AgentLoopTurnUpdate {
        self.compact_before_next_response().await;
        let options = self.effective_prompt_options();
        let context = self.projected_context();
        let update = self.prepare_prompt_and_tool_loadout(&options, &context.messages);
        let (model, thinking_level) = self.with_state(|state| (state.model.clone(), state.thinking_level));
        AgentLoopTurnUpdate {
            context: Some(context),
            messages: update.map(Message::System).into_iter().collect(),
            model: Some(model),
            thinking_level: Some(thinking_level),
        }
    }

    /// Resize oversized images in tool results per `images.autoResize`.
    fn normalize_tool_images(&self, content: &[pi_ai::ContentBlock]) -> Option<Vec<pi_ai::ContentBlock>> {
        let auto_resize = self.settings().image_auto_resize();
        let options = match self.model() {
            Some(model) => ProcessImageOptions::for_model(&model, auto_resize),
            None => ProcessImageOptions { auto_resize, ..ProcessImageOptions::default() },
        };
        normalize_tool_result_images(content, &options)
    }
}

fn prepare_request_hook(weak: Weak<AgentSession>) -> PrepareRequestFn {
    Arc::new(move |_request, _cancel| {
        let weak = weak.clone();
        async move {
            let session = weak.upgrade()?;
            Some(AgentRequestUpdate { context: Some(session.projected_context()), ..AgentRequestUpdate::default() })
        }
        .boxed()
    })
}

fn prepare_next_turn_hook(weak: Weak<AgentSession>) -> PrepareNextTurnFn {
    Arc::new(move |_turn, cancel| {
        let weak = weak.clone();
        async move {
            let session = weak.upgrade()?;
            if cancel.is_cancelled() {
                return None;
            }
            Some(session.prepare_next_turn().await)
        }
        .boxed()
    })
}

fn after_tool_call_hook(weak: Weak<AgentSession>) -> AfterToolCallFn {
    Arc::new(move |call, _cancel| {
        let content = weak.upgrade().and_then(|session| session.normalize_tool_images(&call.result.content));
        async move { content.map(|content| AfterToolCallResult { content: Some(content), ..Default::default() }) }
            .boxed()
    })
}

impl AgentSession {
    /// Install the session's hooks on its agent; `stream_fn` already resolves auth per request.
    pub(crate) fn install_hooks(&self) {
        let weak = self.weak.clone();
        let settings = self.settings().clone();
        let convert: ConvertToLlmFn = Arc::new(move |messages: &[Message]| {
            let converted = crate::messages::convert_to_llm(messages);
            if settings.block_images() { block_images(converted) } else { converted }
        });
        self.agent.set_hooks(AgentHooks {
            stream_fn: Some(self.stream_fn().clone()),
            convert_to_llm: Some(convert),
            prepare_request: Some(prepare_request_hook(weak.clone())),
            prepare_next_turn: Some(prepare_next_turn_hook(weak.clone())),
            after_tool_call: Some(after_tool_call_hook(weak)),
            session_id: Some(self.session_id()),
            ..AgentHooks::default()
        });
    }
}
