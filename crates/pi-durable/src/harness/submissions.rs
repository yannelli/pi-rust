//! Admission, waits, and withdrawal of durable submissions (upstream `harness/submissions.ts`, spec section 6).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use pi_chord::{Context, with_abort_signal};

use super::generation::start_run;
use super::inbox::{
    BoundaryAt, INBOX_DOC, InboxItem, append_user, apply_boundary, entry_draft_json, is_stale, prepare_boundary,
    remove_inbox_item,
};
use super::live::LIVE_DOC;
use super::scheduler::Scheduler;
use super::settings::Settings;
use super::types::{AbortSubmissionResult, SubmissionDraft, WhenBusy};
use super::util::{Waiters, closed_error};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, SubmissionId};
use crate::session::{Session, TransactionScope, Tx};
use crate::types::{
    CommitChange, SubmissionCreate, SubmissionRecord, SubmissionSettlement, SubmissionStatus, SubmissionType,
};

/// Admission, waits, and withdrawal of the durable submissions of one Harness.
pub(crate) struct Submissions {
    session: Session,
    scheduler: Arc<Scheduler>,
    waiters: Waiters<SubmissionId, SubmissionRecord>,
    closed: AtomicBool,
    weak: Weak<Submissions>,
}

impl Submissions {
    pub fn new(session: Session, scheduler: Arc<Scheduler>) -> DurableResult<Arc<Self>> {
        let submissions = Arc::new_cyclic(|weak| Self {
            session: session.clone(),
            scheduler,
            waiters: Waiters::default(),
            closed: AtomicBool::new(false),
            weak: weak.clone(),
        });
        let weak = Arc::downgrade(&submissions);
        session.observe_commits(move |publication, _| {
            let Some(submissions) = weak.upgrade() else { return Ok(()) };
            for change in &publication.changes {
                if let CommitChange::Submission(record) = change
                    && record.status.is_settled()
                {
                    submissions.waiters.resolve(&record.id, record.clone());
                }
            }
            Ok(())
        })?;
        let weak = Arc::downgrade(&submissions);
        session.subscribe_close(move || {
            if let Some(submissions) = weak.upgrade() {
                submissions.closed.store(true, Ordering::SeqCst);
                submissions.waiters.reject_all(closed_error(&submissions.session));
            }
        })?;
        Ok(submissions)
    }

    fn handle(&self, id: SubmissionId) -> DurableResult<Submission> {
        let submissions = self.weak.upgrade().ok_or_else(|| closed_error(&self.session))?;
        Ok(Submission { id, submissions, binding: None })
    }

    /// Admit a submission in one commit; see [`admit_submission`].
    pub async fn submit(&self, conversation_id: ConversationId, draft: SubmissionDraft, context: &Context) -> DurableResult<Submission> {
        self.scheduler.resume();
        let core = self.scheduler.core.clone();
        let id = self
            .session
            .commit_with(
                move |tx| async move { admit_submission(&tx, conversation_id, draft, core.now(), &core.settings()).await },
                context,
                TransactionScope::default(),
            )
            .await?;
        self.handle(id)
    }

    /// Handle for an existing submission, or `None`.
    pub async fn get(&self, id: SubmissionId, context: &Context) -> DurableResult<Option<Submission>> {
        let record = self.read(id, context).await?;
        record.map(|record| self.handle(record.id)).transpose()
    }

    async fn read(&self, id: SubmissionId, context: &Context) -> DurableResult<Option<SubmissionRecord>> {
        let (storage, context) = (self.session.storage(), context.clone());
        self.session.read_on_line(async move { storage.submission(id, &context).await }).await
    }

    pub async fn status(&self, id: SubmissionId, context: &Context) -> DurableResult<SubmissionRecord> {
        self.read(id, context).await?.ok_or_else(|| DurableError::error(format!("Submission {id} does not exist")))
    }

    pub async fn wait(&self, id: SubmissionId, context: &Context) -> DurableResult<SubmissionRecord> {
        self.scheduler.resume();
        let submissions = self.weak.upgrade().ok_or_else(|| closed_error(&self.session))?;
        let (storage, line_context) = (self.session.storage(), context.clone());
        let found = self.session.read_on_line(async move {
            let Some(record) = storage.submission(id, &line_context).await? else {
                return Err(DurableError::error(format!("Submission {id} does not exist")));
            };
            if record.status.is_settled() {
                return Ok(Ok(record));
            }
            if submissions.closed.load(Ordering::SeqCst) {
                return Err(closed_error(&submissions.session));
            }
            Ok(Err(submissions.waiters.add(id, &line_context)))
        });
        match found.await? {
            Ok(record) => Ok(record),
            Err(wait) => wait.await,
        }
    }

    /// Withdraw a queued submission and remove its inbox item; placed inputs and settled submissions are reported.
    pub async fn abort(
        &self,
        id: SubmissionId,
        context: &Context,
        conversation_id: Option<ConversationId>,
    ) -> DurableResult<AbortSubmissionResult> {
        let change = move |tx: Tx| async move {
            let record = tx.submission(id).await?;
            let Some(record) = record.filter(|r| conversation_id.is_none_or(|c| c == r.conversation_id)) else {
                return Ok(AbortSubmissionResult::NotFound);
            };
            match record.status {
                SubmissionStatus::Queued => {
                    let settlement = SubmissionSettlement::Unanswered { reason: "aborted".into(), detail: None };
                    tx.settle_submission(id, settlement)?;
                    remove_inbox_item(&tx, record.conversation_id, id).await?;
                    Ok(AbortSubmissionResult::Aborted)
                }
                SubmissionStatus::Placed => Ok(AbortSubmissionResult::AlreadyPlaced),
                _ => Ok(AbortSubmissionResult::Settled),
            }
        };
        self.session.commit_with(change, context, TransactionScope::default()).await
    }
}

/// Awaitable host handle for one durably admitted submission.
#[derive(Clone)]
pub struct Submission {
    pub id: SubmissionId,
    submissions: Arc<Submissions>,
    binding: Option<Arc<super::scheduler::Invocation>>,
}

impl Submission {
    /// The same submission, its operations bound to a task invocation.
    pub(crate) fn bound(mut self, invocation: Arc<super::scheduler::Invocation>) -> Self {
        self.binding = Some(invocation);
        self
    }

    fn context(&self, context: &Context) -> DurableResult<Context> {
        match &self.binding {
            None => Ok(context.clone()),
            Some(invocation) => {
                invocation.check()?;
                Ok(with_abort_signal(&invocation.controller.signal(), context))
            }
        }
    }

    pub async fn status(&self, context: &Context) -> DurableResult<SubmissionRecord> {
        self.submissions.status(self.id, &self.context(context)?).await
    }

    /// Resolve with the settled record (`done` or `unanswered`).
    pub async fn wait(&self, context: &Context) -> DurableResult<SubmissionRecord> {
        self.submissions.wait(self.id, &self.context(context)?).await
    }

    pub async fn abort(&self, context: &Context) -> DurableResult<AbortSubmissionResult> {
        let result = self.submissions.abort(self.id, &self.context(context)?, None).await?;
        if result == AbortSubmissionResult::NotFound {
            return Err(DurableError::error(format!("Submission {} does not exist", self.id)));
        }
        Ok(result)
    }
}

/// Admit a submission inside a commit (spec section 6). A known request ID returns its submission. A busy
/// conversation queues it, or rejects `reject` input with `ConversationBusy`. An idle one with queued items queues
/// it behind them and runs a final boundary. Otherwise idle input starts a run and an idle write appends its entry.
pub async fn admit_submission(
    tx: &Tx,
    conversation_id: ConversationId,
    draft: SubmissionDraft,
    now: f64,
    settings: &Settings,
) -> DurableResult<SubmissionId> {
    let kind = match draft {
        SubmissionDraft::Input { .. } => SubmissionType::Input,
        SubmissionDraft::Write { .. } => SubmissionType::Write,
    };
    let request_id = draft.request_id().map(str::to_owned);
    if let Some(request) = &request_id
        && let Some(existing) = tx.submission_by_request(conversation_id, request.clone()).await?
    {
        if existing.kind != kind {
            let existing_kind = if existing.kind == SubmissionType::Input { "input" } else { "write" };
            let message = format!("Request {request} already identifies a submission of type {existing_kind}");
            return Err(DurableError::error(message));
        }
        return Ok(existing.id);
    }
    let live_draft = tx.doc(&LIVE_DOC, conversation_id).await?;
    let busy = live_draft.get()?.run.is_some();
    if busy && matches!(draft, SubmissionDraft::Input { when_busy: Some(WhenBusy::Reject), .. }) {
        return Err(DurableError::ConversationBusy(conversation_id));
    }
    // A boundary reads the table, so it is prepared before the first table write; a busy one needs none.
    let mut boundary = if busy { None } else { Some(prepare_boundary(tx, conversation_id, settings).await?) };
    let queued_items = match &boundary {
        Some(boundary) => !boundary.inbox.get()?.items.is_empty(),
        None => true,
    };
    let create = |status, entry| SubmissionCreate {
        status,
        entry,
        ..SubmissionCreate::queued(conversation_id, kind, request_id.clone())
    };
    if queued_items {
        let record = tx.create_submission(create(SubmissionStatus::Queued, None)).await?;
        let item = match draft {
            SubmissionDraft::Write { entry, .. } => InboxItem::Write { id: record.id, entry: entry_draft_json(&entry)? },
            SubmissionDraft::Input { content, when_busy, .. } => match when_busy {
                Some(WhenBusy::Steer) => InboxItem::Steer { id: record.id, content },
                _ => InboxItem::FollowUp { id: record.id, content },
            },
        };
        let inbox = match &boundary {
            Some(boundary) => boundary.inbox.clone(),
            None => tx.doc(&INBOX_DOC, conversation_id).await?,
        };
        inbox.edit(|state| state.items.push(item))?;
        let Some(boundary) = &mut boundary else { return Ok(record.id) };
        let placed = apply_boundary(tx, boundary, BoundaryAt::Final, now).await?;
        if !placed.users.is_empty() {
            start_run(tx, conversation_id, &live_draft, placed.users).await?;
        }
        return Ok(record.id);
    }
    let Some(boundary) = boundary else { unreachable!("an idle conversation has a boundary") };
    match draft {
        SubmissionDraft::Write { entry, .. } => {
            if is_stale(&boundary, &entry) {
                let stale = SubmissionCreate {
                    reason: Some("stale".into()),
                    ..create(SubmissionStatus::Unanswered, None)
                };
                return Ok(tx.create_submission(stale).await?.id);
            }
            let appended = tx.append_entry(conversation_id, entry).await?;
            Ok(tx.create_submission(create(SubmissionStatus::Done, Some(appended.id))).await?.id)
        }
        SubmissionDraft::Input { content, .. } => {
            let entry = append_user(tx, conversation_id, content, now).await?;
            let record = tx.create_submission(create(SubmissionStatus::Placed, Some(entry))).await?;
            start_run(tx, conversation_id, &live_draft, vec![record.id]).await?;
            Ok(record.id)
        }
    }
}
