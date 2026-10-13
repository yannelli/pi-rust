//! Agent loop with tool calling and state management for the pi agent harness.

pub mod agent;
pub mod agent_loop;
pub mod described;
pub mod hooks;
pub mod nested;
pub mod proxy;
pub mod registry;
mod tool_execution;
pub mod tool_model;
pub mod transcript;
pub mod types;
pub mod validate;

pub use agent::{Agent, AgentHooks, AgentInitialState, AgentState, AsyncListener, Listener, QueueMode};
pub use agent_loop::{
    AfterToolCallFn, AgentContext, AgentLoopConfig, ApiKeyFn, BeforeToolCallFn, CallableToolsFn, ConvertToLlmFn,
    EventSink, MessageSource, StreamFn, TransformContextFn, default_convert_to_llm, run_agent_loop,
    run_agent_loop_continue,
};
pub use described::{DescribedTool, with_description};
pub use hooks::{
    AgentLoopTurnUpdate, AgentRequestUpdate, AgentTurnContext, AgentTurnDecision, FinishTurnFn, PrepareNextTurnContext,
    PrepareNextTurnFn, PrepareRequestContext, PrepareRequestFn,
};
pub use nested::{
    ExecuteToolOptions, NESTED_CALL_LIMITS, NestedCallLimits, NestedCallRecorder, NestedCallSummary,
    NestedToolCallHost, NestedToolCallRunner, ToolContext, combine_usage,
};
pub use proxy::{ProxyAssistantMessageEvent, ProxyStreamOptions, proxy_stream_fn, stream_proxy};
pub use registry::{ActivationFilter, RefreshOptions, ToolInfo, ToolRegistry};
pub use tool_execution::{RunToolCallOptions, run_tool_call};
pub use tool_model::{
    ToolAnnotations, ToolExposure, ToolLoadout, ToolLoadoutChanges, ToolNamespace, ToolReplay,
    normalize_prompt_guidelines, normalize_prompt_snippet,
};
pub use types::*;
pub use validate::validate_tool_arguments;
