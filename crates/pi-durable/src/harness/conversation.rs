//! Host handles for one conversation (upstream `ConversationImpl`).

use std::future::Future;

use pi_ai::{Message, UserContent, UserMessage};
use pi_chord::Context;

use super::agent::configure;
use super::compaction::{CompactionInput, create_compaction};
use super::context::read_context;
use super::host::{ConversationInit, Harness};
use super::submissions::Submission;
use super::types::{Agent, AgentChange, CompactionReason, ContextView, SubmissionDraft};
use crate::entries::RESET_ENTRY;
use crate::errors::DurableResult;
use crate::ids::{ConversationId, EntryId, TaskId};
use crate::session::{CommittedWatch, DocumentState, TransactionScope, Tx};
use crate::types::{ConversationOwnership, Cursor, DraftHead, EntryDraft, EntryQuery, EntryRecord, Page, ScanOrder};

/// What a create or fork makes.
pub(crate) enum CreateTarget {
    Root,
    Independent,
    Fork { parent: ConversationId, at: EntryId },
}

/// Options of conversation creation and forks. The conveniences apply in the creating commit, after the creation
/// hook; a root ignores `ownership`.
pub struct CreateOptions {
    pub ownership: ConversationOwnership,
    pub agent: Option<AgentChange>,
    pub init: Option<ConversationInit>,
}

impl CreateOptions {
    pub fn owned(ownership: ConversationOwnership) -> Self {
        Self { ownership, ..Self::default() }
    }

    pub fn with_agent(mut self, agent: AgentChange) -> Self {
        self.agent = Some(agent);
        self
    }

    pub fn with_init(
        mut self,
        init: impl FnOnce(Tx, ConversationId) -> futures::future::BoxFuture<'static, DurableResult<()>> + Send + 'static,
    ) -> Self {
        self.init = Some(Box::new(init));
        self
    }
}

/// Entry scan bounds of [`Conversation::entries`].
#[derive(Clone, Debug, Default)]
pub struct EntryRange {
    pub min_entry_id: Option<EntryId>,
    pub max_entry_id: Option<EntryId>,
    pub order: Option<ScanOrder>,
}

/// Host handle for one conversation; cheap to clone.
#[derive(Clone)]
pub struct Conversation {
    pub id: ConversationId,
    harness: Harness,
}

impl Conversation {
    pub(crate) fn new(id: ConversationId, harness: Harness) -> Self {
        Self { id, harness }
    }

    /// The committed `pi.agent` resolved against the current registry and settings.
    pub async fn agent(&self, context: &Context) -> DurableResult<Agent> {
        self.harness.0.core.resolve_agent(self.id, None, context).await
    }

    pub async fn configure(&self, change: AgentChange, context: &Context) -> DurableResult<()> {
        let id = self.id;
        let change = move |tx: Tx| async move { configure(&tx, id, &change).await };
        self.harness.0.session.commit_with(change, context, TransactionScope::default()).await
    }

    pub async fn submit(&self, draft: SubmissionDraft, context: &Context) -> DurableResult<Submission> {
        self.harness.0.submissions.submit(self.id, draft, context).await
    }

    /// Start a manual compaction; `instructions` steer the summary.
    pub async fn compact(&self, instructions: Option<String>, context: &Context) -> DurableResult<TaskId> {
        self.harness.0.scheduler.resume();
        let (id, input) = (self.id, CompactionInput { reason: CompactionReason::Manual, instructions });
        let change = move |tx: Tx| async move { create_compaction(&tx, id, input, None).await };
        self.harness.0.session.commit_with(change, context, TransactionScope::default()).await
    }

    /// Start a fresh context with a reset entry; `handoff` becomes its user message.
    pub async fn reset(&self, handoff: Option<String>, context: &Context) -> DurableResult<()> {
        let model = handoff.map(|text| {
            let timestamp = self.harness.0.core.now() as i64;
            vec![Message::User(UserMessage { content: UserContent::Text(text), timestamp })]
        });
        let entry = EntryDraft { model, head: Some(DraftHead::SelfEntry), ..EntryDraft::new(RESET_ENTRY.kind()) };
        self.submit(SubmissionDraft::write(entry), context).await?;
        Ok(())
    }

    /// One commit scoped to this conversation.
    pub async fn commit<T, F, Fut>(&self, change: F, context: &Context) -> DurableResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Tx) -> Fut + Send + 'static,
        Fut: Future<Output = DurableResult<T>> + Send + 'static,
    {
        let scope = TransactionScope { conversation_id: Some(self.id), task_id: None };
        self.harness.0.session.commit_with(change, context, scope).await
    }

    /// Committed model context, at `at` when given.
    pub async fn context(&self, context: &Context, at: Option<EntryId>) -> DurableResult<ContextView> {
        read_context(&self.harness.0.session, self.id, context, at).await
    }

    pub async fn entries(
        &self,
        range: EntryRange,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> DurableResult<Page<EntryRecord>> {
        let query = EntryQuery {
            conversation_id: self.id,
            min_entry_id: range.min_entry_id,
            max_entry_id: range.max_entry_id,
            order: range.order,
        };
        let (storage, read_context) = (self.harness.0.session.storage(), context.clone());
        let scan = async move { storage.scan_entries(&query, limit, cursor.as_ref(), &read_context).await };
        self.harness.0.session.read_on_line(scan).await
    }

    pub async fn fork(&self, at: EntryId, options: CreateOptions, context: &Context) -> DurableResult<Conversation> {
        self.harness.create(super::conversation::CreateTarget::Fork { parent: self.id, at }, options, context).await
    }

    /// Abort the conversation's live work; `background` includes background tasks.
    pub async fn abort(&self, context: &Context, background: bool) -> DurableResult<()> {
        self.harness.0.scheduler.resume();
        self.harness.0.scheduler.abort_conversation(self.id, background, context).await
    }

    pub async fn wait_for_idle(&self, context: &Context) -> DurableResult<()> {
        self.harness.0.scheduler.resume();
        self.harness.0.scheduler.wait_for_idle(Some(self.id), context).await
    }

    /// A read-only Chord state of the [`super::ConversationView`].
    pub async fn view_state(&self, context: &Context) -> DurableResult<DocumentState> {
        self.harness.0.views.state(self.id, context).await
    }

    /// A serialized exact-frame watch of the [`super::ConversationView`].
    pub async fn watch(&self, context: &Context) -> DurableResult<CommittedWatch> {
        self.harness.0.views.watch(self.id, context).await
    }
}
