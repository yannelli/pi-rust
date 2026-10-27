//! Admission, waits, and withdrawal of durable submissions (upstream `harness/submissions.ts`, spec section 6).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use pi_chord::{Context, with_abort_signal, with_telemetry};
use pi_telemetry::{
    SpanAttributes, SpanLink, SpanOptions, SpanStatus, TelemetryContext, TelemetryContextExt, TelemetrySpan,
};

use super::generation::start_run;
use super::inbox::{
    BoundaryAt, INBOX_DOC, InboxItem, append_input, apply_boundary, entry_draft_json, has_queued_reset,
    input_entry_json, is_display_only, is_stale, prepare_boundary, remove_inbox_item,
};
use super::live::LIVE_DOC;
use super::scheduler::Scheduler;
use super::settings::Settings;
use super::types::{AbortSubmissionResult, InputEntry, SubmissionDraft, WhenBusy};
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
            // A commit held in memory changes documents only.
            if publication.is_memory() {
                return Ok(());
            }
            for change in publication.changes() {
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
    pub async fn submit(
        &self,
        conversation_id: ConversationId,
        draft: SubmissionDraft,
        context: &Context,
    ) -> DurableResult<Submission> {
        self.scheduler.resume()?;
        let core = self.scheduler.core.clone();
        let input = matches!(draft, SubmissionDraft::Input { .. });
        // Each input starts its own trace, linked to its caller; a write is a child of its caller.
        let parent: Arc<dyn TelemetryContext> = if input { core.telemetry.root.clone() } else { context.telemetry() };
        let attributes = SpanAttributes::new()
            .with("submission.type", if input { "input" } else { "write" })
            .with("conversation.id", conversation_id.get());
        let options = SpanOptions::new("pi.durable.submit").with_attributes(attributes);
        let admitted = parent.start_infallible_span(options, |span| async move {
            if input {
                span.add_link(SpanLink::new(context.telemetry()));
            }
            let admitting = span.clone();
            let admit = move |tx: Tx| async move {
                admit_submission(&tx, conversation_id, draft, core.now(), &core.settings(), Some(&admitting)).await
            };
            let admitted =
                self.session.commit_with(admit, &with_telemetry(span.clone(), context), TransactionScope::default());
            let admitted = admitted.await;
            match &admitted {
                Ok(admission) => span.set_attributes(
                    SpanAttributes::new()
                        .with("submission.id", admission.id.get())
                        .with("result", admission.status.as_str()),
                ),
                // Error text may contain caller content: only the status is recorded.
                Err(_) => span.set_status(SpanStatus::Error { error: None }),
            }
            admitted
        });
        self.handle(admitted.await?.id)
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
        self.scheduler.resume()?;
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

/// What admission did with a submission; a stale write reports `Done`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionStatus {
    Queued,
    Placed,
    Done,
}

impl AdmissionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Placed => "placed",
            Self::Done => "done",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admission {
    pub id: SubmissionId,
    pub status: AdmissionStatus,
}

/// Reject an input entry the caller got wrong: its kind must be non-empty and outside the `pi.` prefix.
fn check_input_entry(entry: &InputEntry) -> DurableResult<()> {
    if entry.kind.is_empty() || entry.kind.starts_with("pi.") {
        let message =
            format!("An input entry's kind must be a non-empty string not starting with \"pi.\", got {}", entry.kind);
        return Err(DurableError::type_error(message));
    }
    Ok(())
}

/// Admit a submission inside a commit (spec section 6). A known request ID returns its submission. A display-only
/// write (only `kind` and `data`) is appended and settled `done` at once, busy or not, as it changes nothing the model
/// sees; only behind a queued reset it queues, so it lands after the reset. A busy conversation queues anything else,
/// or rejects `reject` input with `ConversationBusy`. An idle one with queued items queues it behind them and runs a
/// final boundary. Otherwise idle input places its entry (`pi.user`, or the input's own) and starts a run, and an
/// idle write appends its entry.
/// With `span`, the submission stores its carrier, and a steer links the span to the running generation's trace.
pub async fn admit_submission(
    tx: &Tx,
    conversation_id: ConversationId,
    draft: SubmissionDraft,
    now: f64,
    settings: &Settings,
    span: Option<&Arc<dyn TelemetrySpan>>,
) -> DurableResult<Admission> {
    if let SubmissionDraft::Input { entry: Some(entry), .. } = &draft {
        check_input_entry(entry)?;
    }
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
        let status = match existing.status {
            SubmissionStatus::Queued => AdmissionStatus::Queued,
            SubmissionStatus::Placed => AdmissionStatus::Placed,
            SubmissionStatus::Done | SubmissionStatus::Unanswered => AdmissionStatus::Done,
        };
        return Ok(Admission { id: existing.id, status });
    }
    let propagation = span.and_then(|span| span.propagation());
    let trace = span.zip(propagation.as_ref()).and_then(|(span, propagation)| propagation.inject(span.as_ref()));
    let create = |status, entry| SubmissionCreate {
        status,
        entry,
        trace: trace.clone(),
        ..SubmissionCreate::queued(conversation_id, kind, request_id.clone())
    };
    if let SubmissionDraft::Write { entry, .. } = &draft
        && is_display_only(entry)
        && !has_queued_reset(&tx.doc(&INBOX_DOC, conversation_id).await?.get()?.items)
    {
        let appended = tx.append_entry(conversation_id, entry.clone()).await?;
        let id = tx.create_submission(create(SubmissionStatus::Done, Some(appended.id))).await?.id;
        return Ok(Admission { id, status: AdmissionStatus::Done });
    }
    let live_draft = tx.doc(&LIVE_DOC, conversation_id).await?;
    let run = live_draft.get()?.run;
    let busy = run.is_some();
    if let (Some(span), Some(propagation), Some(run)) = (span, &propagation, &run)
        && matches!(draft, SubmissionDraft::Input { when_busy: Some(WhenBusy::Steer), .. })
        && let Some(trace) = tx.task(run.task_id).await?.and_then(|task| task.trace)
    {
        span.add_link(SpanLink::new(propagation.extract(Some(&trace))));
    }
    if busy && matches!(draft, SubmissionDraft::Input { when_busy: Some(WhenBusy::Reject), .. }) {
        return Err(DurableError::ConversationBusy(conversation_id));
    }
    // A boundary reads the table, so it is prepared before the first table write; a busy one needs none.
    let mut boundary = if busy { None } else { Some(prepare_boundary(tx, conversation_id, settings).await?) };
    let queued_items = match &boundary {
        Some(boundary) => !boundary.inbox.get()?.items.is_empty(),
        None => true,
    };
    if queued_items {
        let record = tx.create_submission(create(SubmissionStatus::Queued, None)).await?;
        let item = match draft {
            SubmissionDraft::Write { entry, .. } => {
                InboxItem::Write { id: record.id, entry: entry_draft_json(&entry)? }
            }
            SubmissionDraft::Input { content, when_busy, entry, .. } => {
                let entry = entry.as_ref().map(input_entry_json);
                match when_busy {
                    Some(WhenBusy::Steer) => InboxItem::Steer { id: record.id, content, entry },
                    _ => InboxItem::FollowUp { id: record.id, content, entry },
                }
            }
        };
        let inbox = match &boundary {
            Some(boundary) => boundary.inbox.clone(),
            None => tx.doc(&INBOX_DOC, conversation_id).await?,
        };
        inbox.edit(|state| state.items.push(item))?;
        let Some(boundary) = &mut boundary else {
            return Ok(Admission { id: record.id, status: AdmissionStatus::Queued });
        };
        let placed = apply_boundary(tx, boundary, BoundaryAt::Final, now).await?;
        let status = match kind {
            SubmissionType::Write => AdmissionStatus::Done,
            _ if placed.users.last() == Some(&record.id) => AdmissionStatus::Placed,
            _ => AdmissionStatus::Queued,
        };
        if !placed.users.is_empty() {
            start_run(tx, conversation_id, &live_draft, placed.users).await?;
        }
        return Ok(Admission { id: record.id, status });
    }
    let Some(boundary) = boundary else { unreachable!("an idle conversation has a boundary") };
    match draft {
        SubmissionDraft::Write { entry, .. } => {
            if is_stale(&boundary, &entry) {
                let stale =
                    SubmissionCreate { reason: Some("stale".into()), ..create(SubmissionStatus::Unanswered, None) };
                let id = tx.create_submission(stale).await?.id;
                return Ok(Admission { id, status: AdmissionStatus::Done });
            }
            let appended = tx.append_entry(conversation_id, entry).await?;
            let id = tx.create_submission(create(SubmissionStatus::Done, Some(appended.id))).await?.id;
            Ok(Admission { id, status: AdmissionStatus::Done })
        }
        SubmissionDraft::Input { content, entry, .. } => {
            let entry = append_input(tx, conversation_id, content, entry.as_ref(), now).await?;
            let record = tx.create_submission(create(SubmissionStatus::Placed, Some(entry))).await?;
            start_run(tx, conversation_id, &live_draft, vec![record.id]).await?;
            Ok(Admission { id: record.id, status: AdmissionStatus::Placed })
        }
    }
}
