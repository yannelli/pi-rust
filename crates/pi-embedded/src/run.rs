//! Running a session: the agent loop with checkpointed events, queues, and tool repair.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use pi_agent::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentRequestUpdate, AgentTurnDecision, EventSink, MessageSource,
    RunToolCallOptions, ToolExecutionMode, run_agent_loop, run_tool_call,
};
use pi_ai::{Message, StopReason, ToolResultMessage, error_stream, now_ms};
use tokio_util::sync::CancellationToken;

use crate::runtime::lock;
use crate::session::{EmbeddedSession, is_failed};
use crate::types::{EventPayload, InvocationPhase, SessionStatus, ToolInvocation, ToolResult};

/// The first error that ends a loop; later loop callbacks become inert.
#[derive(Clone)]
struct Failure {
    error: Arc<Mutex<Option<anyhow::Error>>>,
    cancel: CancellationToken,
}

impl Failure {
    fn fail(&self, error: anyhow::Error) {
        let mut slot = lock(&self.error);
        if slot.is_none() {
            *slot = Some(error);
        }
        self.cancel.cancel();
    }

    fn failed(&self) -> bool {
        lock(&self.error).is_some()
    }

    fn take(&self) -> Option<anyhow::Error> {
        lock(&self.error).take()
    }
}

fn has_uncertain(session: &EmbeddedSession) -> bool {
    lock(&session.inner.state).invocations.iter().any(|item| item.phase == InvocationPhase::Uncertain)
}

impl EmbeddedSession {
    pub(crate) fn start(&self, prepare: Option<Message>) -> BoxFuture<'static, anyhow::Result<()>> {
        let execution = self.spawn_execution(move |session, token| async move {
            let result = session.execute_run(prepare, &token).await;
            session
                .settle(result, &token, |state| {
                    let unresolved =
                        state.needs_response || state.invocations.iter().any(|i| i.phase == InvocationPhase::Uncertain);
                    if unresolved { SessionStatus::Interrupted } else { SessionStatus::Idle }
                })
                .await
        });
        Box::pin(async move { execution.await.map_err(|error| anyhow::anyhow!(error)) })
    }

    /// Record a failed run, then checkpoint the final status unless storage failed.
    pub(crate) async fn settle(
        &self,
        result: anyhow::Result<()>,
        token: &CancellationToken,
        status: impl FnOnce(&crate::SessionSnapshot) -> SessionStatus,
    ) -> Result<(), String> {
        let mut outcome = Ok(());
        if let Err(error) = result {
            outcome = match self.storage_error() {
                Some(storage) => Err(storage),
                None => {
                    self.change(|state| {
                        state.error = Some(error.to_string());
                        state.needs_response = true;
                        Ok(())
                    })
                    .await
                }
            };
        }
        if self.storage_error().is_none() {
            let stopping = *lock(&self.inner.stopping);
            let aborted = token.is_cancelled();
            let finished = self
                .change_with(
                    |state| {
                        state.status = if aborted { stopping } else { status(state) };
                        Ok(())
                    },
                    |state| Some(EventPayload::Session { status: state.status, error: state.error.clone() }),
                )
                .await;
            if let Err(error) = finished {
                outcome = Err(error);
            }
        }
        outcome.map_err(|error| error.to_string())
    }

    async fn execute_run(&self, prepare: Option<Message>, token: &CancellationToken) -> anyhow::Result<()> {
        if let Some(message) = prepare {
            self.change(|state| {
                state.messages.push(message);
                state.needs_response = true;
                Ok(())
            })
            .await?;
        }
        let running = |state: &mut crate::SessionSnapshot| {
            state.status = SessionStatus::Running;
            state.error = None;
            Ok(())
        };
        self.change_with(running, |_| Some(EventPayload::Session { status: SessionStatus::Running, error: None }))
            .await?;
        self.repair_tools(token).await?;
        let idle = {
            let state = lock(&self.inner.state);
            !state.needs_response && state.steering.is_empty() && state.follow_ups.is_empty()
        };
        if idle || token.is_cancelled() {
            return Ok(());
        }
        self.run_loop(token).await
    }

    async fn run_loop(&self, token: &CancellationToken) -> anyhow::Result<()> {
        let loop_token = token.child_token();
        let failure = Failure { error: Arc::default(), cancel: loop_token.clone() };
        let model = lock(&self.inner.state).model.clone();
        let mut config = AgentLoopConfig::new(model);
        config.tool_execution = ToolExecutionMode::Sequential;
        config.convert_to_llm = Some(Arc::new(|messages: &[Message]| messages.to_vec()));
        let host_stream = self.inner.host.host.stream.clone();
        let gate = failure.clone();
        config.stream_fn = Some(Arc::new(move |model, context, options| {
            if gate.failed() {
                return error_stream(model, StopReason::Aborted, "Execution cancelled");
            }
            host_stream(model, context, options)
        }));
        config.get_steering_messages = Some(self.drain_source(true, &failure));
        config.get_follow_up_messages = Some(self.drain_source(false, &failure));
        let (session, gate) = (self.clone(), failure.clone());
        config.prepare_request = Some(Arc::new(move |_request, _cancel| {
            let (session, gate) = (session.clone(), gate.clone());
            Box::pin(async move {
                match session.prepare_request(&gate.cancel).await {
                    Ok(update) => Some(update),
                    Err(error) => {
                        gate.fail(error);
                        None
                    }
                }
            })
        }));
        let (session, gate) = (self.clone(), failure.clone());
        config.finish_turn = Some(Arc::new(move |_turn, _cancel| {
            let (session, gate) = (session.clone(), gate.clone());
            Box::pin(async move {
                if let Some(error) = session.storage_error() {
                    gate.fail(error);
                    return Some(AgentTurnDecision::End);
                }
                let stop = gate.cancel.is_cancelled() || has_uncertain(&session);
                stop.then_some(AgentTurnDecision::End)
            })
        }));
        let (session, gate) = (self.clone(), failure.clone());
        let sink: EventSink = Arc::new(move |event| {
            let (session, gate) = (session.clone(), gate.clone());
            Box::pin(async move {
                if !gate.failed()
                    && let Err(error) = session.on_agent_event(event).await
                {
                    gate.fail(error);
                }
                None
            })
        });
        let mut context =
            AgentContext { system_prompt: None, messages: self.context(None), tools: self.agent_tools(&loop_token) };
        run_agent_loop(Vec::new(), &mut context, &config, sink, loop_token).await;
        match failure.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn drain_source(&self, steering: bool, failure: &Failure) -> MessageSource {
        let (session, gate) = (self.clone(), failure.clone());
        Arc::new(move || {
            let (session, gate) = (session.clone(), gate.clone());
            Box::pin(async move {
                if gate.failed() {
                    return Vec::new();
                }
                session.drain(steering, &gate.cancel).await.unwrap_or_else(|error| {
                    gate.fail(error);
                    Vec::new()
                })
            })
        })
    }

    async fn prepare_request(&self, cancel: &CancellationToken) -> anyhow::Result<AgentRequestUpdate> {
        if cancel.is_cancelled() {
            anyhow::bail!("Execution cancelled");
        }
        if has_uncertain(self) {
            anyhow::bail!("Tool outcome requires reconciliation");
        }
        if let Some(limit) = self.inner.host.host.compact_at_characters
            && serde_json::to_string(&self.context(None))?.len() > limit
        {
            self.compact_context(cancel).await?;
        }
        self.change(|state| {
            state.needs_response = true;
            Ok(())
        })
        .await?;
        let context =
            AgentContext { system_prompt: None, messages: self.context(None), tools: self.agent_tools(cancel) };
        let model = lock(&self.inner.state).model.clone();
        Ok(AgentRequestUpdate { context: Some(context), model: Some(model), thinking_level: None })
    }

    async fn drain(&self, steering: bool, cancel: &CancellationToken) -> anyhow::Result<Vec<Message>> {
        let empty = {
            let state = lock(&self.inner.state);
            if steering { state.steering.is_empty() } else { state.follow_ups.is_empty() }
        };
        if cancel.is_cancelled() || empty {
            return Ok(Vec::new());
        }
        let drained = Arc::new(Mutex::new(Vec::new()));
        let taken = drained.clone();
        self.change_with(
            |state| {
                let queue = if steering { &mut state.steering } else { &mut state.follow_ups };
                let messages = std::mem::take(queue);
                state.messages.extend(messages.iter().cloned());
                state.needs_response = true;
                *lock(&taken) = messages;
                Ok(())
            },
            |state| {
                Some(EventPayload::Queue { steering: state.steering.clone(), follow_ups: state.follow_ups.clone() })
            },
        )
        .await?;
        // prepare_request supplies the durably updated transcript to the loop.
        Ok(std::mem::take(&mut *lock(&drained)))
    }

    pub(crate) async fn on_agent_event(&self, event: AgentEvent) -> anyhow::Result<()> {
        let tools = self.inner.host.available_tools();
        let stable = event.clone();
        self.change_with(
            |state| {
                match &event {
                    AgentEvent::MessageStart { message: Message::Assistant(message) }
                    | AgentEvent::MessageUpdate { message: Message::Assistant(message), .. } => {
                        state.partial = Some(message.clone());
                    }
                    AgentEvent::MessageEnd { message } => record_message(state, message, &tools)?,
                    _ => {}
                }
                Ok(())
            },
            |_| Some(EventPayload::Agent { event: stable }),
        )
        .await?;
        if let AgentEvent::MessageEnd { message: Message::Assistant(message) } = event {
            let usage = message.usage;
            let record = |state: &mut crate::SessionSnapshot| {
                state.usage.push(usage);
                Ok(())
            };
            self.change_with(record, |_| Some(EventPayload::Usage { usage })).await?;
        }
        Ok(())
    }

    async fn repair_tools(&self, token: &CancellationToken) -> anyhow::Result<()> {
        let messages = lock(&self.inner.state).messages.clone();
        for message in messages {
            let Message::Assistant(assistant) = &message else { continue };
            if is_failed(&message) {
                continue;
            }
            for call in assistant.tool_calls() {
                let answered = lock(&self.inner.state)
                    .messages
                    .iter()
                    .any(|item| matches!(item, Message::ToolResult(result) if result.tool_call_id == call.id));
                if answered {
                    continue;
                }
                if token.is_cancelled() {
                    return Ok(());
                }
                let invocation = lock(&self.inner.state).invocations.iter().find(|item| item.id == call.id).cloned();
                let phase = invocation.as_ref().map(|item| item.phase);
                if phase == Some(InvocationPhase::Uncertain) {
                    anyhow::bail!("Tool outcome requires reconciliation");
                }
                let result = match invocation
                    .and_then(|item| item.result.filter(|_| phase == Some(InvocationPhase::Completed)))
                {
                    Some(result) => result,
                    None if assistant.stop_reason == StopReason::Length => {
                        ToolResult::failure("Truncated tool arguments; tool was not executed")
                    }
                    None => {
                        let tools = self.agent_tools(token);
                        let context =
                            AgentContext { system_prompt: None, messages: self.context(None), tools: Vec::new() };
                        let options = RunToolCallOptions {
                            tools: &tools,
                            assistant_message: assistant,
                            context: &context,
                            before_tool_call: None,
                            after_tool_call: None,
                            parent_tool_call_id: None,
                            tool_context: None,
                            cancel: token.clone(),
                            on_update: None,
                        };
                        ToolResult::from_agent(run_tool_call(call.clone(), options).await.result)
                    }
                };
                let message = ToolResultMessage {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    is_error: result.is_error(),
                    content: result.content,
                    details: Some(result.details),
                    timestamp: now_ms(),
                    ..ToolResultMessage::default()
                };
                self.on_agent_event(AgentEvent::MessageEnd { message: Message::ToolResult(message) }).await?;
                if has_uncertain(self) {
                    anyhow::bail!("Tool outcome requires reconciliation");
                }
            }
        }
        Ok(())
    }
}

/// Persist a finalized loop message and track the invocations it starts or completes.
fn record_message(
    state: &mut crate::SessionSnapshot,
    message: &Message,
    tools: &[Arc<dyn crate::EmbeddedTool>],
) -> anyhow::Result<()> {
    match message {
        Message::User(_) => {}
        Message::Assistant(assistant) => {
            state.partial = None;
            state.messages.push(message.clone());
            let calls: Vec<_> = assistant.tool_calls().cloned().collect();
            let failed = is_failed(message);
            state.needs_response = !calls.is_empty() || failed;
            if failed {
                return Ok(());
            }
            for call in calls {
                if state.invocations.iter().any(|item| item.id == call.id) {
                    anyhow::bail!("Provider reused tool invocation id");
                }
                let tool = tools.iter().find(|tool| tool.declaration().name == call.name);
                state.invocations.push(ToolInvocation {
                    session_id: state.id.clone(),
                    id: call.id.clone(),
                    name: call.name.clone(),
                    args: call.arguments.clone(),
                    effect: tool.map(|tool| tool.declaration().effect).unwrap_or_default(),
                    phase: InvocationPhase::Pending,
                    result: None,
                    approved: None,
                });
            }
        }
        Message::ToolResult(result) => {
            state.messages.push(message.clone());
            let invocation = state.invocations.iter_mut().find(|item| item.id == result.tool_call_id);
            if let Some(invocation) = invocation.filter(|item| item.phase != InvocationPhase::Uncertain) {
                invocation.phase = InvocationPhase::Completed;
                invocation.result.get_or_insert_with(|| ToolResult {
                    content: result.content.clone(),
                    details: result.details.clone().unwrap_or_default(),
                    is_error: Some(result.is_error),
                });
            }
        }
        _ => state.messages.push(message.clone()),
    }
    Ok(())
}
