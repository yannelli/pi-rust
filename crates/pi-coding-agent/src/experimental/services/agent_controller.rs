//! The presentation-safe `AgentController` service over the worker-owned root conversation (upstream
//! `services/agent-controller.ts` and `agent-controller-provider.ts`).

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use pi_ai::{ContentBlock, Message, UserContent};
use pi_chord::services::{ServiceHandle, ServiceImpl, ServiceRef};
use pi_chord::{ChordError, ChordResult, Context, Json};
use pi_durable::harness::{AbortSubmissionResult, Conversation, Harness, SubmissionDraft, WhenBusy};
use pi_durable::{DurableError, SubmissionId, SubmissionStatus, SubmissionType};
use serde::{Deserialize, Serialize};

use super::codec::{decode_arg, decode_result, encode};

pub static AGENT_CONTROLLER: LazyLock<ServiceRef> = LazyLock::new(|| ServiceRef::remote("pi.agent-controller"));

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPromptImage {
    #[serde(rename = "type")]
    pub kind: String,
    pub data: String,
    pub mime_type: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptRequest {
    pub message: String,
    pub images: Option<Vec<AgentPromptImage>>,
}

impl AgentPromptRequest {
    pub fn text(message: impl Into<String>) -> Self {
        Self { message: message.into(), images: None }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentOperationError {
    pub code: String,
    pub message: String,
}

/// `operationId` identifies the durable submission of a prompt, or the task of a compaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOperationResponse {
    pub accepted: bool,
    pub operation_id: Option<String>,
    pub error: Option<AgentOperationError>,
}

/// `entryId` identifies the durable submission; `cancel_queued` withdraws it while it is still queued.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentQueueResponse {
    pub accepted: bool,
    pub entry_id: Option<String>,
    pub error: Option<AgentOperationError>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CancelQueuedResult {
    /// `cancelled`, `already_consumed`, or `not_found`.
    pub outcome: String,
}

/// The settled outcome of a prompt: the answer's text, or why it got none.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentPromptResult {
    /// `done` or `unanswered`.
    pub status: String,
    pub text: Option<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCompactionRequest {
    pub custom_instructions: Option<String>,
}

#[async_trait]
pub trait AgentController: Send + Sync {
    /// Start a run; rejected with `busy` while one is active.
    async fn prompt(&self, request: AgentPromptRequest, context: &Context) -> ChordResult<AgentOperationResponse>;
    /// Steer the active run, or start one when idle.
    async fn steer(&self, request: AgentPromptRequest, context: &Context) -> ChordResult<AgentQueueResponse>;
    /// Queue input for after the active run, or start one when idle.
    async fn follow_up(&self, request: AgentPromptRequest, context: &Context) -> ChordResult<AgentQueueResponse>;
    async fn cancel_queued(&self, entry_id: String, context: &Context) -> ChordResult<CancelQueuedResult>;
    /// Withdraw queued input and abort the active run and compaction.
    async fn abort(&self, context: &Context) -> ChordResult<()>;
    async fn compact(&self, request: AgentCompactionRequest, context: &Context) -> ChordResult<AgentOperationResponse>;
    /// Wait until the prompt with this `operation_id` is answered or settles unanswered.
    async fn wait_for_prompt(&self, operation_id: String, context: &Context) -> ChordResult<AgentPromptResult>;
}

macro_rules! method {
    ($impl:expr, $service:expr, $name:literal, |$s:ident, $args:ident, $context:ident| $body:expr) => {{
        let service = $service.clone();
        $impl.method($name, move |$args: Vec<Json>, $context: Context| {
            let $s = service.clone();
            async move { $body }
        })
    }};
}

pub fn agent_controller_impl(service: Arc<dyn AgentController>) -> ServiceImpl {
    let mut implementation = ServiceImpl::new();
    implementation = method!(implementation, service, "prompt", |s, args, context| {
        encode(&s.prompt(decode_arg(&args, 0)?, &context).await?).map(Some)
    });
    implementation = method!(implementation, service, "steer", |s, args, context| {
        encode(&s.steer(decode_arg(&args, 0)?, &context).await?).map(Some)
    });
    implementation = method!(implementation, service, "followUp", |s, args, context| {
        encode(&s.follow_up(decode_arg(&args, 0)?, &context).await?).map(Some)
    });
    implementation = method!(implementation, service, "cancelQueued", |s, args, context| {
        encode(&s.cancel_queued(decode_arg(&args, 0)?, &context).await?).map(Some)
    });
    implementation = method!(implementation, service, "abort", |s, args, context| {
        drop(args);
        s.abort(&context).await.map(|()| None)
    });
    implementation = method!(implementation, service, "compact", |s, args, context| {
        encode(&s.compact(decode_arg(&args, 0)?, &context).await?).map(Some)
    });
    method!(implementation, service, "waitForPrompt", |s, args, context| {
        encode(&s.wait_for_prompt(decode_arg(&args, 0)?, &context).await?).map(Some)
    })
}

/// Calls `pi.agent-controller` through a service handle.
#[derive(Clone)]
pub struct AgentControllerClient(pub ServiceHandle);

#[async_trait]
impl AgentController for AgentControllerClient {
    async fn prompt(&self, request: AgentPromptRequest, context: &Context) -> ChordResult<AgentOperationResponse> {
        decode_result(self.0.call("prompt", vec![encode(&request)?], context).await?)
    }
    async fn steer(&self, request: AgentPromptRequest, context: &Context) -> ChordResult<AgentQueueResponse> {
        decode_result(self.0.call("steer", vec![encode(&request)?], context).await?)
    }
    async fn follow_up(&self, request: AgentPromptRequest, context: &Context) -> ChordResult<AgentQueueResponse> {
        decode_result(self.0.call("followUp", vec![encode(&request)?], context).await?)
    }
    async fn cancel_queued(&self, entry_id: String, context: &Context) -> ChordResult<CancelQueuedResult> {
        decode_result(self.0.call("cancelQueued", vec![Json::from(entry_id)], context).await?)
    }
    async fn abort(&self, context: &Context) -> ChordResult<()> {
        self.0.call("abort", Vec::new(), context).await.map(drop)
    }
    async fn compact(&self, request: AgentCompactionRequest, context: &Context) -> ChordResult<AgentOperationResponse> {
        decode_result(self.0.call("compact", vec![encode(&request)?], context).await?)
    }
    async fn wait_for_prompt(&self, operation_id: String, context: &Context) -> ChordResult<AgentPromptResult> {
        decode_result(self.0.call("waitForPrompt", vec![Json::from(operation_id)], context).await?)
    }
}

/// The controller over one Harness and its root conversation.
pub fn create_agent_controller(harness: Harness, conversation: Conversation) -> Arc<dyn AgentController> {
    Arc::new(DurableAgentController { harness, conversation })
}

struct DurableAgentController {
    harness: Harness,
    conversation: Conversation,
}

impl DurableAgentController {
    async fn submit(
        &self,
        request: AgentPromptRequest,
        mode: WhenBusy,
        context: &Context,
    ) -> Result<String, AgentOperationError> {
        let draft =
            SubmissionDraft::Input { request_id: None, content: to_input(request), when_busy: Some(mode), entry: None };
        match self.conversation.submit(draft, context).await {
            Ok(submission) => Ok(submission.id.get().to_string()),
            Err(error) => Err(to_agent_error(&error)),
        }
    }

    async fn queue(&self, request: AgentPromptRequest, mode: WhenBusy, context: &Context) -> AgentQueueResponse {
        match self.submit(request, mode, context).await {
            Ok(id) => AgentQueueResponse { accepted: true, entry_id: Some(id), error: None },
            Err(error) => AgentQueueResponse { accepted: false, entry_id: None, error: Some(error) },
        }
    }
}

fn operation(result: Result<String, AgentOperationError>) -> AgentOperationResponse {
    match result {
        Ok(id) => AgentOperationResponse { accepted: true, operation_id: Some(id), error: None },
        Err(error) => AgentOperationResponse { accepted: false, operation_id: None, error: Some(error) },
    }
}

#[async_trait]
impl AgentController for DurableAgentController {
    async fn prompt(&self, request: AgentPromptRequest, context: &Context) -> ChordResult<AgentOperationResponse> {
        Ok(operation(self.submit(request, WhenBusy::Reject, context).await))
    }
    async fn steer(&self, request: AgentPromptRequest, context: &Context) -> ChordResult<AgentQueueResponse> {
        Ok(self.queue(request, WhenBusy::Steer, context).await)
    }
    async fn follow_up(&self, request: AgentPromptRequest, context: &Context) -> ChordResult<AgentQueueResponse> {
        Ok(self.queue(request, WhenBusy::FollowUp, context).await)
    }
    async fn cancel_queued(&self, entry_id: String, context: &Context) -> ChordResult<CancelQueuedResult> {
        let Some(id) = parse_submission_id(&entry_id) else {
            return Ok(CancelQueuedResult { outcome: "not_found".into() });
        };
        let result = self.harness.abort_submission(id, context, Some(self.conversation.id)).await?;
        let outcome = match result {
            AbortSubmissionResult::Aborted => "cancelled",
            AbortSubmissionResult::NotFound => "not_found",
            AbortSubmissionResult::AlreadyPlaced | AbortSubmissionResult::Settled => "already_consumed",
        };
        Ok(CancelQueuedResult { outcome: outcome.into() })
    }
    async fn abort(&self, context: &Context) -> ChordResult<()> {
        Ok(self.conversation.abort(context, false).await?)
    }
    async fn compact(&self, request: AgentCompactionRequest, context: &Context) -> ChordResult<AgentOperationResponse> {
        let result = self.conversation.compact(request.custom_instructions, context).await;
        Ok(operation(result.map(|id| id.get().to_string()).map_err(|error| to_agent_error(&error))))
    }
    async fn wait_for_prompt(&self, operation_id: String, context: &Context) -> ChordResult<AgentPromptResult> {
        let submission = match parse_submission_id(&operation_id) {
            Some(id) => self.harness.submission(id, context).await?,
            None => None,
        };
        let Some(submission) = submission else {
            return Err(ChordError::error(format!("Unknown prompt: {operation_id}")));
        };
        let settled = submission.wait(context).await?;
        if settled.status == SubmissionStatus::Unanswered {
            let reason = settled.reason.unwrap_or_default();
            return Ok(AgentPromptResult { status: "unanswered".into(), text: None, reason: Some(reason) });
        }
        let answer = if settled.kind == SubmissionType::Input { settled.answer } else { None };
        let entry = match answer {
            Some(answer) => {
                self.harness.session().commit(move |tx| async move { tx.entry(answer).await }, context).await?
            }
            None => None,
        };
        let message = entry.and_then(|entry| entry.model).and_then(|messages| messages.into_iter().next());
        let text = match message {
            Some(Message::Assistant(message)) => message.content.iter().filter_map(ContentBlock::as_text).collect(),
            _ => String::new(),
        };
        Ok(AgentPromptResult { status: "done".into(), text: Some(text), reason: None })
    }
}

fn parse_submission_id(value: &str) -> Option<SubmissionId> {
    let valid = value.starts_with(|c: char| ('1'..='9').contains(&c)) && value.chars().all(|c| c.is_ascii_digit());
    let id = value.parse::<u64>().ok().filter(|id| valid && *id <= 9_007_199_254_740_991)?;
    Some(SubmissionId::new(id))
}

fn to_input(request: AgentPromptRequest) -> UserContent {
    match request.images.filter(|images| !images.is_empty()) {
        None => UserContent::Text(request.message),
        Some(images) => {
            let mut content = vec![ContentBlock::text(request.message)];
            content.extend(
                images.into_iter().map(|image| ContentBlock::Image { data: image.data, mime_type: image.mime_type }),
            );
            UserContent::Blocks(content)
        }
    }
}

fn to_agent_error(error: &DurableError) -> AgentOperationError {
    match error {
        DurableError::ConversationBusy(_) => AgentOperationError { code: "busy".into(), message: error.to_string() },
        _ => AgentOperationError { code: "operation_failed".into(), message: error.to_string() },
    }
}
