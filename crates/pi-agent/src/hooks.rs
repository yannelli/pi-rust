//! Turn and request hooks: `finish_turn`, `prepare_request`, and `prepare_next_turn`.
//! Port of the upstream `FinishTurn`, `PrepareRequest`, and `prepareNextTurn` callbacks.

use std::sync::Arc;

use futures::future::BoxFuture;
use pi_ai::{AssistantMessage, Message, Model, ThinkingLevel, ToolResultMessage};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::AgentContext;

/// A completed turn: the assistant message, its tool results, and the context after both.
#[derive(Clone, Copy)]
pub struct AgentTurnContext<'a> {
    pub message: &'a AssistantMessage,
    pub tool_results: &'a [ToolResultMessage],
    /// Context after the turn's assistant message and tool results were appended.
    pub context: &'a AgentContext,
    /// What the loop returns if it exits now. Prompt runs include the prompts;
    /// continuation runs leave out messages that were already in the context.
    pub new_messages: &'a [Message],
}

/// Input to `prepare_next_turn`.
pub type PrepareNextTurnContext<'a> = AgentTurnContext<'a>;

/// Returned by `finish_turn`. `None` keeps normal scheduling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentTurnDecision {
    /// Make sure one more provider request runs. Tool results, steering, or follow-up
    /// messages satisfy it; otherwise the loop runs once more with the current context.
    Continue,
    /// End the run without polling queues or preparing another request.
    End,
}

/// Replacement state applied before the next turn. Unset fields keep current values.
#[derive(Clone, Default)]
pub struct AgentLoopTurnUpdate {
    pub context: Option<AgentContext>,
    /// Appended before the next request, with normal message events.
    pub messages: Vec<Message>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

/// Runtime state right before a provider request.
#[derive(Clone, Copy)]
pub struct PrepareRequestContext<'a> {
    pub context: &'a AgentContext,
    pub model: &'a Model,
    pub thinking_level: ThinkingLevel,
}

/// Replacement state for the request being prepared and later requests in the run.
#[derive(Clone, Default)]
pub struct AgentRequestUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

impl From<AgentRequestUpdate> for AgentLoopTurnUpdate {
    fn from(update: AgentRequestUpdate) -> Self {
        Self {
            context: update.context,
            messages: Vec::new(),
            model: update.model,
            thinking_level: update.thinking_level,
        }
    }
}

/// Called after a turn's assistant message and tool results, before `turn_end`.
/// Error and aborted responses still end the run whatever this returns.
pub type FinishTurnFn = Arc<
    dyn for<'a> Fn(AgentTurnContext<'a>, CancellationToken) -> BoxFuture<'a, Option<AgentTurnDecision>> + Send + Sync,
>;

/// Called right before every provider request, including the first, after pending
/// messages were appended. Does not poll the queues.
pub type PrepareRequestFn = Arc<
    dyn for<'a> Fn(PrepareRequestContext<'a>, CancellationToken) -> BoxFuture<'a, Option<AgentRequestUpdate>>
        + Send
        + Sync,
>;

/// Called after `turn_end` when the loop continues, right before the next turn starts.
pub type PrepareNextTurnFn = Arc<
    dyn for<'a> Fn(PrepareNextTurnContext<'a>, CancellationToken) -> BoxFuture<'a, Option<AgentLoopTurnUpdate>>
        + Send
        + Sync,
>;
