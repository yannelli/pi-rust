//! Upstream cases on who continues: a host after a crash, a tool through the handle of a conversation it owns, and a
//! caller whose trace the continue links.

use std::time::Duration;

use pi_chord::with_telemetry;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{FauxTokenSize, RegisterFauxProviderOptions};
use pi_telemetry::{
    AttributeValue, InMemoryTelemetryContext, RecordedTelemetryLink, SpanAttributes, SpanOptions, SpanOutcome,
    TelemetryContext,
};

use super::support::ControlledStorage;
use super::support::chat::{add_tool, call, no_parameters};
use super::*;

/// Open (or reopen after a crash) a chat over `storage` and start scheduling.
async fn open(storage: &Arc<ControlledStorage>, setup: &ChatSetup) -> (Harness, Conversation) {
    storage.reopen();
    let (harness, root) = open_chat(storage.clone(), setup).await;
    harness.resume().unwrap();
    (harness, root)
}

/// A crash: from now on nothing reaches Storage.
async fn kill(harness: &Harness) {
    harness.fail(DurableError::error("crash"), None);
    let closed = tokio::time::timeout(Duration::from_secs(10), harness.closed()).await;
    closed.expect("a crashed Harness kept running");
}

fn text(value: &str) -> AttributeValue {
    AttributeValue::String(value.into())
}

#[tokio::test]
async fn answers_a_run_interrupted_by_a_host_that_died_while_the_model_answered() {
    let storage = Arc::new(ControlledStorage::default());
    let token_size = Some(FauxTokenSize { min: Some(1), max: Some(1) });
    let options = RegisterFauxProviderOptions { tokens_per_second: Some(20.0), token_size, ..Default::default() };
    let first = chat_setup(options);
    first.faux.set_responses([faux_assistant_message("z".repeat(400), FauxMessageOptions::default())]);
    first.settings.lock().unwrap().progress =
        Some(ProgressOverrides { persist_within_ms: Some(0.0), ..Default::default() });
    let (harness, root) = open(&storage, &first).await;
    let id = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().id;
    wait_for(|| async { live(&harness, &root).await["generation"].get("message").is_some() }).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    kill(&harness).await;

    let setup = chat_setup(Default::default());
    setup.settings.lock().unwrap().retry = Some(RetryOverrides { enabled: Some(false), ..RetryOverrides::default() });
    let seen = Seen::default();
    setup.faux.set_responses([answering("after the crash", &seen)]);
    let (harness, root) = open(&storage, &setup).await;
    let interrupted = harness.submission(id, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(reason(&interrupted), (SubmissionStatus::Unanswered, Some("interrupted")));
    let settled = continued(&root).await;
    assert_eq!(answer_text(&root, &settled).await.as_deref(), Some("after the crash"));
    // The aborted partial is not model context: the request ends in the user message.
    assert_eq!(last_roles(&seen, 1), ["user"]);
    harness.close(&ctx()).await.unwrap();
}

/// What the `delegate` tool saw of its child conversation.
#[derive(Debug, Default)]
struct Delegated {
    first: Option<SubmissionStatus>,
    busy: bool,
    resumed: Option<SubmissionStatus>,
}

#[tokio::test]
async fn is_submitted_by_a_tool_through_the_handle_of_a_conversation_it_owns_and_rejected_there_while_busy() {
    let setup = chat_setup(Default::default());
    let (busy, release) = (Deferred::default(), Deferred::default());
    let outcome: Arc<Mutex<Delegated>> = Arc::default();
    let (recorded, waiting, releasing) = (outcome.clone(), busy.clone(), release.clone());
    add_tool(
        &setup,
        define_tool("delegate", "Delegates", no_parameters(), move |_, api, context| {
            let (recorded, waiting, releasing) = (recorded.clone(), waiting.clone(), releasing.clone());
            async move {
                let owner = api.task_id;
                let create = move |tx: Tx| async move {
                    let created = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
                    configure(&tx, created.id, &AgentChange::model(faux_model())).await?;
                    Ok(created.id)
                };
                let child = api.commit(create, &context).await?;
                let handle = api.conversation(child, &context).await?.expect("the child conversation");
                let first = handle.submit(SubmissionDraft::input("work"), &context).await?.wait(&context).await?;
                let resumed = handle.submit(SubmissionDraft::continue_run(), &context).await?;
                waiting.wait().await;
                let rejected = handle.submit(SubmissionDraft::continue_run(), &context).await;
                releasing.resolve();
                let settled = resumed.wait(&context).await?;
                *recorded.lock().unwrap() = Delegated {
                    first: Some(first.status),
                    busy: matches!(rejected, Err(DurableError::ConversationBusy(id)) if id == child),
                    resumed: Some(settled.status),
                };
                Ok(ToolExecutionResult::text(settled.status.as_str()))
            }
        }),
    );
    let (entered, held) = (busy.clone(), release.clone());
    let recovering = FauxResponseStep::async_factory(move |_, _, _, _| {
        let (entered, held) = (entered.clone(), held.clone());
        Box::pin(async move {
            entered.resolve();
            held.wait().await;
            Ok(faux_assistant_message("child recovered", FauxMessageOptions::default()))
        })
    });
    let done: FauxResponseStep = faux_assistant_message("done", FauxMessageOptions::default()).into();
    setup.faux.set_responses([call("delegate", "c1"), failure(), recovering, done]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    {
        let outcome = outcome.lock().unwrap();
        assert_eq!(
            (outcome.first, outcome.resumed),
            (Some(SubmissionStatus::Unanswered), Some(SubmissionStatus::Done))
        );
        assert!(outcome.busy, "{outcome:?}");
    }
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn starts_a_root_trace_linked_to_its_caller_as_an_input_does() {
    let setup = chat_setup(Default::default());
    let telemetry = InMemoryTelemetryContext::new();
    setup.faux.set_responses([failure(), faux_assistant_message("recovered", FauxMessageOptions::default()).into()]);
    let options = setup.options().with_telemetry(Arc::new(telemetry.clone()));
    let (harness, root) = open_chat_with(Arc::new(MemoryStorage::new()), &setup, options).await;
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let caller = telemetry.open_span(SpanOptions::new("caller"));
    let caller_context = telemetry.get_spans().into_iter().find(|span| span.name == "caller").unwrap().context;
    let call_context = with_telemetry(caller.clone(), &ctx());
    let resumed = root.submit(SubmissionDraft::continue_run(), &call_context).await.unwrap();
    resumed.wait(&call_context).await.unwrap();
    caller.end(SpanOutcome::Ok);
    harness.wait_for_idle(&ctx()).await.unwrap();
    wait_for(|| async { telemetry.get_spans().iter().all(|span| span.settled) }).await;

    let spans = telemetry.get_spans();
    let id = AttributeValue::from(resumed.id.get());
    let submit =
        spans.iter().find(|span| span.name == "pi.durable.submit" && span.attributes.get("submission.id") == Some(&id));
    let submit = submit.expect("the continue's submit span");
    assert_eq!(submit.parent_id, None);
    assert_eq!(submit.attributes.get("submission.type"), Some(&text("continue")));
    assert_eq!(submit.attributes.get("result"), Some(&text("placed")));
    let link = RecordedTelemetryLink { context: caller_context, attributes: SpanAttributes::new() };
    assert_eq!(submit.links, [link]);
    let record = harness.submission(resumed.id, &ctx()).await.unwrap().unwrap().status(&ctx()).await.unwrap();
    assert_eq!(record.trace.as_ref(), Some(&submit.context));
    let mut prepares = spans
        .iter()
        .filter(|span| span.name == "pi.durable.phase" && span.attributes.get("phase") == Some(&text("prepare")));
    assert!(prepares.any(|span| span.parent_context.as_ref() == Some(&submit.context)));
    harness.close(&ctx()).await.unwrap();
}
