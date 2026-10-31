//! Failed attempts after host interruption, and terminal cleanup at the consecutive-restart limit.

use pi_ai::retry::retry_delay_ms;
use pi_chord::{Context, Json};

use super::{
    DeferredRetryPoll, GenerationCheckpoint, convert_partial, decode, end_failed_run, failed, prepare_failure,
    sleeping, unanswered,
};
use crate::errors::{DurableError, DurableResult};
use crate::harness::live::{LIVE_DOC, LiveGeneration, RetryState, RunFailureReason, cut_off_count, salvageable};
use crate::harness::scheduler::TaskRuntime;
use crate::models::DeferredCancelOptions;
use crate::session::Tx;
use crate::types::{HostCallback, ReportFrom, TaskRecord};

pub(super) fn deferred_of(checkpoint: &GenerationCheckpoint) -> Option<DeferredRetryPoll> {
    match checkpoint {
        GenerationCheckpoint::Poll { model, thinking_level, cutoff, handle, .. } => Some(DeferredRetryPoll {
            model: model.clone(),
            thinking_level: *thinking_level,
            cutoff: *cutoff,
            handle: handle.clone(),
        }),
        GenerationCheckpoint::Retry { poll, .. } => poll.clone(),
        _ => None,
    }
}

pub(super) async fn cancel_deferred(runtime: &TaskRuntime, poll: &DeferredRetryPoll, context: &Context) {
    let Some(model) = runtime.models().get_model(&poll.model.provider, &poll.model.model_id) else { return };
    let options = DeferredCancelOptions {
        signal: Some(runtime.signal()),
        telemetry: Some(context.telemetry()),
        ..DeferredCancelOptions::default()
    };
    if let Err(error) = runtime.models().cancel_deferred(&model, &poll.handle, options).await {
        runtime.report_from(&error, ReportFrom::Host(HostCallback::Models));
    }
}

pub(super) async fn interrupted(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let checkpoint = decode(&task)?;
    let (attempt, compacted) = match &checkpoint {
        GenerationCheckpoint::Request { attempt, compacted, .. }
        | GenerationCheckpoint::Poll { attempt, compacted, .. } => (*attempt, *compacted),
        _ => return Err(DurableError::error("Interrupted generation is not in request or poll")),
    };
    let policy = runtime.settings().retry;
    let retry = policy.enabled && attempt <= policy.max_retries;
    let until = if retry {
        runtime.now() + retry_delay_ms(policy.base_delay_ms, policy.max_agent_delay_ms, attempt) as f64
    } else {
        0.0
    };
    let poll = deferred_of(&checkpoint);
    let text = if poll.is_some() {
        format!("The host stopped while the deferred answer was fetched, on attempt {attempt}")
    } else {
        format!("The host stopped while the model was answering, on attempt {attempt}")
    };
    if !retry && let Some(poll) = &poll {
        cancel_deferred(&runtime, poll, &context).await;
    }
    let runtime_ = runtime.clone();
    runtime
        .commit(
            move |tx: Tx, _| async move {
                let failure = if retry { None } else { Some(prepare_failure(&tx, &runtime_).await?) };
                let draft = tx.doc(&LIVE_DOC, runtime_.conversation_id()).await?;
                let mut live = draft.get()?;
                convert_partial(&tx, &mut live, runtime_.conversation_id()).await?;
                if let Some(failure) = failure {
                    end_failed_run(&tx, &runtime_, failure, unanswered(RunFailureReason::Interrupted, Some(&text)))
                        .await?;
                    return Ok(failed(&text, "interrupted"));
                }
                live.generation = Some(LiveGeneration {
                    retry: Some(RetryState { at: until, error: text }),
                    ..LiveGeneration::new(attempt)
                });
                draft.set(&live)?;
                sleeping(&GenerationCheckpoint::Retry { attempt, compacted, until, poll }, until)
            },
            &context,
        )
        .await
}

pub(super) async fn end_cut_off(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let checkpoint = decode(&task)?;
    let phase = task
        .state
        .checkpoint()
        .and_then(|checkpoint| checkpoint.get("phase"))
        .and_then(Json::as_str)
        .unwrap_or_default();
    let text = format!("The generation was interrupted {} in its {phase} phase", cut_off_count(&task));
    let salvage = salvageable(&task, &runtime);
    if salvage && let Some(poll) = deferred_of(&checkpoint) {
        cancel_deferred(&runtime, &poll, &context).await;
    }
    let runtime_ = runtime.clone();
    runtime
        .commit(
            move |tx: Tx, _| async move {
                let failure = prepare_failure(&tx, &runtime_).await?;
                if salvage {
                    let draft = tx.doc(&LIVE_DOC, runtime_.conversation_id()).await?;
                    let mut live = draft.get()?;
                    convert_partial(&tx, &mut live, runtime_.conversation_id()).await?;
                }
                end_failed_run(&tx, &runtime_, failure, unanswered(RunFailureReason::RestartLimit, Some(&text)))
                    .await?;
                Ok(failed(&text, "restart_limit"))
            },
            &context,
        )
        .await
}
