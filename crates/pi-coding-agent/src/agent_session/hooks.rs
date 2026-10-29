//! Agent hooks of a session (upstream `_installAgentRequestProjection`,
//! `_installAgentNextTurnRefresh`, and `_afterToolCall`): every request uses the session
//! projection, turns compact and declare prompt changes in between, and tool images are resized.

use std::sync::{Arc, Weak};

use futures::FutureExt;
use pi_agent::transcript::hide_tool_declarations;
use pi_agent::{
    AfterToolCallFn, AgentContext, AgentHooks, AgentLoopTurnUpdate, AgentTurnDecision, BeforeToolCallFn,
    CallableToolsFn, ConvertToLlmFn, FinishTurnFn, PrepareNextTurnFn, PrepareRequestFn, TransformContextFn,
};
use pi_ai::Message;

use super::{AgentSession, lock};
use crate::images::{ProcessImageOptions, block_images, normalize_tool_result_images};

impl AgentSession {
    /// The request context: the session projection with the active tools.
    pub(crate) fn projected_context(&self) -> AgentContext {
        let messages = self.session_manager().build_session_context().messages;
        let tools = self.with_state(|state| state.tools.clone());
        AgentContext { system_prompt: None, messages, tools }
    }

    /// Between turns of one run: compact when the threshold is crossed, then declare prompt
    /// and tool changes made during the run in a system message before the next request.
    async fn prepare_next_turn(&self) -> AgentLoopTurnUpdate {
        self.compact_before_next_response().await;
        self.mcp_turn_start().await;
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
        let options = match self.limits_model() {
            Some(model) => ProcessImageOptions::for_model(&model, auto_resize),
            None => ProcessImageOptions { auto_resize, ..ProcessImageOptions::default() },
        };
        normalize_tool_result_images(content, &options)
    }
}

fn prepare_request_hook(weak: Weak<AgentSession>) -> PrepareRequestFn {
    Arc::new(move |_request, cancel| {
        let weak = weak.clone();
        async move {
            let session = weak.upgrade()?;
            Some(session.prepare_request_update(cancel).await)
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

/// `tool_result` handlers, then image normalization of the final content.
fn after_tool_call_hook(weak: Weak<AgentSession>) -> AfterToolCallFn {
    Arc::new(move |call, _cancel| {
        let weak = weak.clone();
        async move {
            let session = weak.upgrade()?;
            let hooked = session.extension_tool_result(&call).await;
            let content = hooked.as_ref().and_then(|hooked| hooked.content.clone());
            let base = content.clone().unwrap_or_else(|| call.result.content.clone());
            let normalized = session.normalize_tool_images(&base);
            if hooked.is_none() && normalized.is_none() {
                return None;
            }
            let mut result = hooked.unwrap_or_default();
            result.content = normalized.or(content);
            Some(result)
        }
        .boxed()
    })
}

/// `turn_end` boundary handlers decide whether the run continues (upstream `finishTurn`).
fn finish_turn_hook(weak: Weak<AgentSession>) -> FinishTurnFn {
    Arc::new(move |turn, _cancel| {
        let weak = weak.clone();
        async move {
            let session = weak.upgrade()?;
            let continued = session.finish_turn_boundary(turn.message, turn.tool_results).await;
            continued.then_some(AgentTurnDecision::Continue)
        }
        .boxed()
    })
}

/// Nested calls resolve against the callable tools: the active `direct` tools and every
/// `codemode` or `deferred` tool.
fn callable_tools_hook(weak: Weak<AgentSession>) -> CallableToolsFn {
    Arc::new(move || {
        weak.upgrade()
            .map(|session| session.with_tool_registry(|registry| registry.callable_tools()))
            .unwrap_or_default()
    })
}

/// Requests leave out the declarations `prepare_loadout` hooks hide (upstream
/// `_installHiddenDeclarationsProjection`).
fn transform_context_hook(weak: Weak<AgentSession>) -> TransformContextFn {
    Arc::new(move |messages, _cancel| {
        let weak = weak.clone();
        async move {
            let Some(session) = weak.upgrade() else { return messages };
            let messages = session.extension_context(messages).await;
            let messages = hide_tool_declarations(messages, &session.hidden_declarations());
            let forced = lock(&session.state).run_prompt.as_ref().and_then(|run| run.force_system_prompt.clone());
            match forced {
                Some(forced) => force_system_prompt(messages, forced),
                None => messages,
            }
        }
        .boxed()
    })
}

/// A `before_agent_start` handler's `systemPrompt` as the request's only system message, with
/// the current tool declarations; the transcript keeps its sections (upstream
/// `_installAgentForcedPromptProjection`).
fn force_system_prompt(messages: Vec<Message>, forced: String) -> Vec<Message> {
    let current = pi_ai::transcript::current_system_message(&messages);
    let head = pi_ai::SystemMessage {
        content: pi_ai::UserContent::Text(forced),
        sections: None,
        tools_added: current.as_ref().and_then(|current| current.tools_added.clone()),
        tools_removed: None,
        timestamp: current.as_ref().map_or_else(pi_ai::now_ms, |current| current.timestamp),
    };
    let rest = messages.into_iter().filter(|message| !matches!(message, Message::System(_)));
    std::iter::once(Message::System(head)).chain(rest).collect()
}

/// A call waits for the MCP servers it needs (upstream MCP `tool_call` handler).
/// Extension `tool_call` handlers run after the wait and may block or edit the call.
fn before_tool_call_hook(weak: Weak<AgentSession>) -> BeforeToolCallFn {
    Arc::new(move |call, cancel| {
        let weak = weak.clone();
        async move {
            let session = weak.upgrade()?;
            if let Some(manager) = session.mcp_manager() {
                let name = &call.tool_call.name;
                // Outside the tools lock: an extension's `reachesTools` runs JavaScript.
                let reach = session.tool(name).and_then(|tool| tool.reaches_tools(call.args));
                manager.wait_for_tool_call(name, reach.as_ref(), &cancel).await;
            }
            let result = session.extension_tool_call(&call).await;
            if result.as_ref().is_some_and(|result| result.block) {
                return result;
            }
            let authorize = lock(&session.tool_authorizer).clone();
            if let Some(authorize) = authorize {
                let args = result.as_ref().and_then(|result| result.args.as_ref()).unwrap_or(call.args);
                let checked = pi_agent::BeforeToolCall { args, ..call };
                if let Some(denied) = authorize(checked, cancel).await {
                    return Some(denied);
                }
            }
            result
        }
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
            stream_fn: Some(AgentSession::loop_stream_fn(weak.clone())),
            convert_to_llm: Some(convert),
            prepare_request: Some(prepare_request_hook(weak.clone())),
            prepare_next_turn: Some(prepare_next_turn_hook(weak.clone())),
            before_tool_call: Some(before_tool_call_hook(weak.clone())),
            after_tool_call: Some(after_tool_call_hook(weak.clone())),
            transform_context: Some(transform_context_hook(weak.clone())),
            callable_tools: Some(callable_tools_hook(weak.clone())),
            finish_turn: Some(finish_turn_hook(weak.clone())),
            on_event: Some(super::extension_hooks::agent_event_hook(weak)),
            session_id: Some(self.session_id()),
            ..AgentHooks::default()
        });
    }
}
