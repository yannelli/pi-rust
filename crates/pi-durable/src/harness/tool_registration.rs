//! Executable tools registered in a registry (upstream `ToolRegistration` and `defineTool`).

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use futures::future::BoxFuture;
use pi_ai::{JsonObject, Tool};
use pi_chord::Context;
use serde_json::Value;

use super::tool::ToolExecutionApi;
use super::types::{ToolExecutionMode, ToolExecutionResult};
use crate::errors::DurableResult;

/// Whether an interrupted execution may rerun on recovery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolReplay {
    Safe,
    #[default]
    Unsafe,
}

impl ToolReplay {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Unsafe => "unsafe",
        }
    }
}

/// Who may call a tool: the model, other tools through `execute_tool()`, or both (default).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolCaller {
    Model,
    Tools,
}

/// Which end of a long output a tool keeps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Retain {
    #[default]
    Head,
    Tail,
}

impl Retain {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Head => "head",
            Self::Tail => "tail",
        }
    }
}

/// A tool's output limits; unset fields take the defaults (50 KiB, 2000 lines, head).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ToolOutputLimits {
    pub max_bytes: Option<usize>,
    pub max_lines: Option<usize>,
    pub retain: Option<Retain>,
}

pub type ExecuteFn =
    Arc<dyn Fn(JsonObject, ToolExecutionApi, Context) -> BoxFuture<'static, DurableResult<ToolExecutionResult>> + Send + Sync>;
pub type PrepareArgumentsFn = Arc<dyn Fn(&Value) -> DurableResult<Value> + Send + Sync>;

/// Executable tool. Only the pi-ai `Tool` fields enter the transcript; `args` are validated against `parameters`.
#[derive(Clone)]
pub struct ToolRegistration {
    pub tool: Tool,
    /// Default `Unsafe`.
    pub replay: Option<ToolReplay>,
    /// Default: the settings' `tool_execution`. One sequential call makes its whole round sequential.
    pub execution_mode: Option<ToolExecutionMode>,
    /// Repair arguments before validation. Must be pure: it runs again when a call is retried.
    pub prepare_arguments: Option<PrepareArgumentsFn>,
    /// Schema of `structured_output`; `None`: programs get the bounded output.
    pub structured_output_schema: Option<Value>,
    /// `None`: both callers.
    pub callers: Option<Vec<ToolCaller>>,
    pub output_limits: Option<ToolOutputLimits>,
    pub execute: ExecuteFn,
}

impl fmt::Debug for ToolRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolRegistration").field("name", &self.tool.name).field("replay", &self.replay).finish()
    }
}

impl ToolRegistration {
    pub fn name(&self) -> &str {
        &self.tool.name
    }

    /// Whether `caller` may call this tool.
    pub fn callable_by(&self, caller: ToolCaller) -> bool {
        self.callers.as_ref().is_none_or(|callers| callers.contains(&caller))
    }

    pub fn with_replay(mut self, replay: ToolReplay) -> Self {
        self.replay = Some(replay);
        self
    }

    pub fn with_execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.execution_mode = Some(mode);
        self
    }

    pub fn with_callers(mut self, callers: Vec<ToolCaller>) -> Self {
        self.callers = Some(callers);
        self
    }

    pub fn with_output_limits(mut self, limits: ToolOutputLimits) -> Self {
        self.output_limits = Some(limits);
        self
    }

    pub fn with_structured_output_schema(mut self, schema: Value) -> Self {
        self.structured_output_schema = Some(schema);
        self
    }

    pub fn with_prepare_arguments(
        mut self,
        prepare: impl Fn(&Value) -> DurableResult<Value> + Send + Sync + 'static,
    ) -> Self {
        self.prepare_arguments = Some(Arc::new(prepare));
        self
    }

    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

/// Define a tool from its declaration and an async `execute`.
pub fn define_tool<F, Fut>(
    name: impl Into<String>,
    description: impl Into<String>,
    parameters: Value,
    execute: F,
) -> ToolRegistration
where
    F: Fn(JsonObject, ToolExecutionApi, Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = DurableResult<ToolExecutionResult>> + Send + 'static,
{
    ToolRegistration {
        tool: Tool::new(name, description, parameters),
        replay: None,
        execution_mode: None,
        prepare_arguments: None,
        structured_output_schema: None,
        callers: None,
        output_limits: None,
        execute: Arc::new(move |args, api, context| Box::pin(execute(args, api, context))),
    }
}
