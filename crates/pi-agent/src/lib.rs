//! Agent loop with tool calling and state management for the pi agent harness.

pub mod agent;
pub mod agent_loop;
pub mod hooks;
mod tool_execution;
pub mod transcript;
pub mod types;
pub mod validate;

pub use agent::{Agent, AgentHooks, AgentInitialState, AgentState, Listener, QueueMode};
pub use agent_loop::{
    AfterToolCallFn, AgentContext, AgentLoopConfig, ApiKeyFn, BeforeToolCallFn, ConvertToLlmFn, EventSink,
    MessageSource, StreamFn, TransformContextFn, default_convert_to_llm, run_agent_loop, run_agent_loop_continue,
};
pub use hooks::{
    AgentLoopTurnUpdate, AgentRequestUpdate, AgentTurnContext, AgentTurnDecision, FinishTurnFn, PrepareNextTurnContext,
    PrepareNextTurnFn, PrepareRequestContext, PrepareRequestFn,
};
pub use types::*;
pub use validate::validate_tool_arguments;
