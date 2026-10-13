//! Tool execution: approval waits, durable intent before effects, and uncertain outcomes.

use std::sync::Arc;

use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, SharedTool, ToolUpdateCallback};
use pi_ai::JsonObject;
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::host::{EmbeddedTool, ToolContext};
use crate::runtime::lock;
use crate::session::EmbeddedSession;
use crate::types::{EventPayload, InvocationPhase, SessionStatus, ToolEffect, ToolResult};

const DENIED: &str = "Host denied or cancelled tool execution";
const CANCELLED: &str = "Execution cancelled before tool start";

/// An embedded tool presented to the agent loop for one run.
struct SessionTool {
    session: EmbeddedSession,
    tool: Arc<dyn EmbeddedTool>,
    cancel: CancellationToken,
}

#[async_trait]
impl AgentTool for SessionTool {
    fn name(&self) -> &str {
        &self.tool.declaration().name
    }

    fn description(&self) -> &str {
        &self.tool.declaration().description
    }

    fn parameters(&self) -> Value {
        self.tool.declaration().parameters.clone()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        args: Value,
        _cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let args = match args {
            Value::Object(args) => args,
            _ => JsonObject::new(),
        };
        let (result, terminate) = self.session.execute_tool(&self.tool, tool_call_id, args, &self.cancel).await?;
        Ok(result.into_agent(terminate))
    }
}

impl EmbeddedSession {
    pub(crate) fn agent_tools(&self, cancel: &CancellationToken) -> Vec<SharedTool> {
        self.inner
            .host
            .available_tools()
            .into_iter()
            .map(|tool| Arc::new(SessionTool { session: self.clone(), tool, cancel: cancel.clone() }) as SharedTool)
            .collect()
    }

    async fn set_phase(&self, id: &str, phase: InvocationPhase) -> anyhow::Result<()> {
        self.change(|state| {
            if let Some(current) = state.invocations.iter_mut().find(|item| item.id == id) {
                current.phase = phase;
            }
            Ok(())
        })
        .await
    }

    /// Wait for the host decision; cancellation counts as a denial.
    async fn await_approval(&self, id: &str, args: &JsonObject, cancel: &CancellationToken) -> anyhow::Result<bool> {
        let (sender, receiver) = oneshot::channel();
        lock(&self.inner.approvals).insert(id.to_string(), sender);
        let requested = self
            .change_with(
                |state| {
                    state.status = SessionStatus::AwaitingApproval;
                    if let Some(current) = state.invocations.iter_mut().find(|item| item.id == id) {
                        current.phase = InvocationPhase::Approval;
                        current.args = args.clone();
                    }
                    Ok(())
                },
                |state| {
                    let invocation = state.invocations.iter().find(|item| item.id == id).cloned()?;
                    Some(EventPayload::Approval { invocation })
                },
            )
            .await;
        let decision = match requested {
            Ok(()) => tokio::select! {
                decision = receiver => decision.unwrap_or(false),
                _ = cancel.cancelled() => false,
            },
            Err(error) => {
                lock(&self.inner.approvals).remove(id);
                return Err(error);
            }
        };
        lock(&self.inner.approvals).remove(id);
        Ok(decision && !cancel.is_cancelled())
    }

    /// Returns the result and whether it ends the loop (an uncertain mutation).
    pub(crate) async fn execute_tool(
        &self,
        tool: &Arc<dyn EmbeddedTool>,
        id: &str,
        args: JsonObject,
        cancel: &CancellationToken,
    ) -> anyhow::Result<(ToolResult, bool)> {
        let declaration = tool.declaration();
        let invocation = lock(&self.inner.state).invocations.iter().find(|item| item.id == id).cloned();
        let invocation = match invocation {
            Some(invocation) if invocation.phase != InvocationPhase::Uncertain => invocation,
            _ => anyhow::bail!("Tool outcome requires reconciliation"),
        };
        if invocation.phase == InvocationPhase::Completed {
            return Ok((invocation.result.unwrap_or_default(), false));
        }
        if cancel.is_cancelled() {
            return Ok((ToolResult::failure(CANCELLED), false));
        }
        let reviewed = declaration.approval || declaration.effect == ToolEffect::Mutation;
        if reviewed && invocation.approved.is_none() && !self.await_approval(id, &args, cancel).await? {
            return Ok((ToolResult::failure(DENIED), false));
        }
        let approved = lock(&self.inner.state).invocations.iter().find(|item| item.id == id).and_then(|i| i.approved);
        if approved == Some(false) || cancel.is_cancelled() {
            return Ok((ToolResult::failure(DENIED), false));
        }
        self.change(|state| {
            state.status = SessionStatus::Running;
            if let Some(current) = state.invocations.iter_mut().find(|item| item.id == id) {
                current.phase = InvocationPhase::Executing;
                current.args = args.clone();
            }
            Ok(())
        })
        .await?;
        if cancel.is_cancelled() {
            self.set_phase(id, InvocationPhase::Pending).await?;
            return Ok((ToolResult::failure(CANCELLED), false));
        }
        let progress_session = self.clone();
        let invocation_id = id.to_string();
        let progress = Arc::new(move |result: ToolResult| {
            let (session, invocation_id) = (progress_session.clone(), invocation_id.clone());
            Box::pin(async move {
                let payload = EventPayload::ToolProgress { invocation_id, result };
                session.change_with(|_| Ok(()), |_| Some(payload)).await
            }) as futures::future::BoxFuture<'static, anyhow::Result<()>>
        });
        let context = ToolContext::new(self.id(), id, cancel.clone(), progress);
        let result = match tool.execute(args, context).await {
            Ok(result) => result,
            Err(error) => {
                let failure = ToolResult::failure(error.to_string());
                if declaration.effect == ToolEffect::Mutation {
                    self.set_phase(id, InvocationPhase::Uncertain).await?;
                    self.abort_controller();
                    return Ok((failure, true));
                }
                failure
            }
        };
        self.change(|state| {
            if let Some(current) = state.invocations.iter_mut().find(|item| item.id == id) {
                current.phase = InvocationPhase::Completed;
                current.result = Some(result.clone());
            }
            Ok(())
        })
        .await?;
        Ok((result, false))
    }
}
