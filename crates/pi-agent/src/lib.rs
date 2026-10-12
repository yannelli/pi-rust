//! Agent loop with tool calling and state management for the pi agent harness.

pub mod agent;
pub mod agent_loop;
mod tool_execution;
pub mod types;
pub mod validate;

pub use agent::{Agent, AgentHooks, AgentState, Listener, QueueMode};
pub use agent_loop::{
    AfterToolCallFn, AgentContext, AgentLoopConfig, ApiKeyFn, BeforeToolCallFn, ConvertToLlmFn, EventSink,
    MessageSource, StreamFn, TransformContextFn, default_convert_to_llm, run_agent_loop, run_agent_loop_continue,
};
pub use types::*;
pub use validate::validate_tool_arguments;
