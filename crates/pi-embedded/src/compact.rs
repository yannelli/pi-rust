//! Manual and automatic compaction: summarize older context, keep the full transcript.

use tokio_util::sync::CancellationToken;

use crate::runtime::lock;
use crate::session::EmbeddedSession;
use crate::types::{EventPayload, InvocationPhase, SessionStatus};

impl EmbeddedSession {
    /// Summarize older context while idle or paused, preserving the latest user turn.
    pub async fn compact(&self) -> anyhow::Result<()> {
        self.assert_open()?;
        if self.running().is_some() {
            anyhow::bail!("Compact while the session is idle or paused");
        }
        let previous = {
            let state = lock(&self.inner.state);
            if state.invocations.iter().any(|item| item.phase != InvocationPhase::Completed) {
                anyhow::bail!("Resolve tool outcomes before compacting");
            }
            state.status
        };
        let execution = self.spawn_execution(move |session, token| async move {
            let running = |state: &mut crate::SessionSnapshot| {
                state.status = SessionStatus::Running;
                Ok(())
            };
            let event = |_: &crate::SessionSnapshot| {
                Some(EventPayload::Session { status: SessionStatus::Running, error: None })
            };
            let mut outcome = session.change_with(running, event).await;
            if outcome.is_ok() && !token.is_cancelled() {
                outcome = session.compact_context(&token).await;
            }
            let failure = match outcome {
                Err(error) if !token.is_cancelled() || session.storage_error().is_some() => Some(error.to_string()),
                _ => None,
            };
            if session.storage_error().is_none() {
                let stopping = *lock(&session.inner.stopping);
                let status = if token.is_cancelled() { stopping } else { previous };
                let settle = |state: &mut crate::SessionSnapshot| {
                    state.status = status;
                    Ok(())
                };
                let event =
                    |state: &crate::SessionSnapshot| Some(EventPayload::Session { status: state.status, error: None });
                session.change_with(settle, event).await.map_err(|error| error.to_string())?;
            }
            failure.map_or(Ok(()), Err)
        });
        execution.await.map_err(|error| anyhow::anyhow!(error))
    }

    pub(crate) async fn compact_context(&self, cancel: &CancellationToken) -> anyhow::Result<()> {
        let Some(compactor) = self.inner.host.host.compactor.clone() else {
            anyhow::bail!("Host did not provide a compactor");
        };
        let (boundary, model) = {
            let state = lock(&self.inner.state);
            let mut boundary = state.messages.len().saturating_sub(1);
            while boundary > state.context_start && state.messages[boundary].role() != "user" {
                boundary -= 1;
            }
            if boundary <= state.context_start {
                return Ok(());
            }
            (boundary, state.model.clone())
        };
        let prior = self.context(Some(boundary));
        let compacted = compactor.compact(prior, model, cancel.clone()).await?;
        if cancel.is_cancelled() {
            anyhow::bail!("Compaction cancelled");
        }
        if compacted.summary.trim().is_empty() {
            anyhow::bail!("Compactor returned an empty summary");
        }
        let summary = compacted.summary;
        let event_summary = summary.clone();
        self.change_with(
            |state| {
                state.context_start = boundary;
                state.context_summary = Some(summary);
                Ok(())
            },
            |_| Some(EventPayload::Compaction { context_start: boundary, summary: event_summary }),
        )
        .await?;
        if let Some(usage) = compacted.usage {
            let record = |state: &mut crate::SessionSnapshot| {
                state.usage.push(usage);
                Ok(())
            };
            self.change_with(record, |_| Some(EventPayload::Usage { usage })).await?;
        }
        Ok(())
    }
}
