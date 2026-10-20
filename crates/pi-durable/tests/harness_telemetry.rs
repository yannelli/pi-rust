//! Port of upstream `test/harness-telemetry.test.ts`: Durable's spans, events, carriers, and metrics.

mod support;

use std::sync::Arc;

use pi_chord::{Json, with_telemetry};
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::tasks::{TaskOptions, TaskTrace};
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message, faux_text,
};
use pi_durable::*;
use pi_telemetry::{
    AttributeValue, InMemoryTelemetryContext, RecordedTelemetrySpan, SpanAttributes, SpanOptions, SpanOutcome,
    TelemetryCarrier, TelemetryContext,
};
use serde_json::{Value, json};
use support::chat::{answer, calls};
use support::harness_support::*;
use support::task_support::{completed, open_tasks_with};
use tokio::sync::Notify;

fn tracked(setup: &ChatSetup, telemetry: &InMemoryTelemetryContext) -> HarnessOptions {
    setup.options().with_telemetry(Arc::new(telemetry.clone()))
}

fn attribute<'a>(span: &'a RecordedTelemetrySpan, name: &str) -> Option<&'a AttributeValue> {
    span.attributes.get(name)
}

fn text(span: &RecordedTelemetrySpan, name: &str) -> Option<String> {
    match attribute(span, name) {
        Some(AttributeValue::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn phases(telemetry: &InMemoryTelemetryContext) -> Vec<RecordedTelemetrySpan> {
    telemetry.get_spans().into_iter().filter(|span| span.name == "pi.durable.phase").collect()
}

fn submits(telemetry: &InMemoryTelemetryContext) -> Vec<RecordedTelemetrySpan> {
    telemetry.get_spans().into_iter().filter(|span| span.name == "pi.durable.submit").collect()
}

fn has_event(span: &RecordedTelemetrySpan, name: &str, attributes: Value) -> bool {
    let attributes: SpanAttributes = serde_json::from_value(attributes).unwrap();
    span.events.iter().any(|event| event.name == name && event.attributes == attributes)
}

async fn settled(telemetry: &InMemoryTelemetryContext) {
    wait_for(|| async { telemetry.get_spans().iter().all(|span| span.settled) }).await;
}

/// Every recorded span and event uses the schema's names and attribute types.
fn assert_schema(telemetry: &InMemoryTelemetryContext) {
    for span in telemetry.get_spans() {
        let definition = &DURABLE_TELEMETRY_SCHEMA.spans[&span.name];
        for event in &span.events {
            definition.check_event(&event.name, &event.attributes).unwrap();
        }
    }
}

/// A tool whose output is private; with `nested`, it first calls that tool with a private argument.
fn tool(name: &str, nested: Option<&'static str>) -> ToolRegistration {
    let parameters = json!({ "type": "object", "properties": { "secret": { "type": "string" } } });
    define_tool(name, name, parameters, move |_, api, context| async move {
        if let Some(nested) = nested {
            let args = json!({ "secret": "private-argument" }).as_object().cloned().unwrap();
            api.execute_tool(nested, args, &context, Default::default()).await?;
        }
        Ok(ToolExecutionResult::text("private-output"))
    })
}

#[tokio::test]
async fn records_content_free_submit_and_phase_spans_nested_parents_checkpoints_entry_events_and_metrics() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let telemetry = InMemoryTelemetryContext::new();
    let install = |registration: ToolRegistration| {
        let name = format!("tool:{}", registration.name());
        setup.registry.install(Extension::new(name).tool(registration)).unwrap();
    };
    install(tool("inner", None));
    install(tool("outer", Some("inner")));
    setup.faux.set_responses(vec![calls(&[("outer", json!({}), "call-1")]), answer("private-answer")]);
    let (harness, root) = open_chat_with(Arc::new(MemoryStorage::new()), &setup, tracked(&setup, &telemetry)).await;
    let draft = SubmissionDraft::input("private-input").with_request_id("dedupe");
    let submitted = root.submit(draft, &ctx()).await.unwrap();
    submitted.wait(&ctx()).await.unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    settled(&telemetry).await;
    let record = submitted.status(&ctx()).await.unwrap();
    let again = SubmissionDraft::input("another-private-input").with_request_id("dedupe");
    let again = root.submit(again, &ctx()).await.unwrap();
    assert_eq!(again.status(&ctx()).await.unwrap(), record);

    let submit = submits(&telemetry).remove(0);
    assert_eq!(submit.parent_id, None);
    assert_eq!(attribute(&submit, "submission.id"), Some(&AttributeValue::from(submitted.id.get())));
    assert_eq!(text(&submit, "result").as_deref(), Some("placed"));
    assert_eq!(record.trace.as_ref(), Some(&submit.context));
    let entry = json!({ "entry.id": record.entry.unwrap().get(), "entry.kind": "pi.user" });
    assert!(has_event(&submit, "pi.durable.entry", entry), "{:?}", submit.events);

    let phases = phases(&telemetry);
    assert!(phases.len() > 4, "{phases:?}");
    assert_eq!(phases[0].parent_context.as_ref(), Some(&submit.context));
    assert_eq!(text(&phases[0], "phase").as_deref(), Some("prepare"));
    assert_eq!(attribute(&phases[0], "resumed"), Some(&AttributeValue::Boolean(false)));
    assert_eq!(text(&phases[0], "outcome").as_deref(), Some("next=request"));
    let outer = phases.iter().find(|span| text(span, "tool.name").as_deref() == Some("outer")).unwrap();
    let inner = phases.iter().find(|span| text(span, "tool.name").as_deref() == Some("inner")).unwrap();
    assert_eq!(inner.parent_context.as_ref(), Some(&outer.context));
    assert_eq!(text(outer, "phase").as_deref(), Some("call"));
    assert_eq!(text(outer, "outcome").as_deref(), Some("done"));
    assert_eq!(text(outer, "tool.call_id").as_deref(), Some("call-1"));
    assert!(has_event(outer, "pi.durable.checkpoint", json!({ "phase": "execute" })), "{:?}", outer.events);
    assert!(phases.iter().any(|span| text(span, "outcome").as_deref() == Some("waiting")));
    let settlement = json!({
        "submission.id": submitted.id.get(),
        "status": "done",
        "answer.entry_id": record.answer.unwrap().get(),
    });
    assert!(phases.iter().any(|span| has_event(span, "pi.durable.submission", settlement.clone())));
    assert_schema(&telemetry);
    let recorded = serde_json::to_string(&telemetry.get_spans()).unwrap();
    for content in ["private-input", "private-answer", "private-output", "private-argument", "secret"] {
        assert!(!recorded.contains(content), "{content} leaked");
    }

    harness.close(&ctx()).await.unwrap();
    let metrics = telemetry.get_metrics();
    let durations = metrics.iter().filter(|metric| metric.name == "pi.durable.phase.duration").count();
    assert_eq!(durations, phases.len());
    let active: f64 =
        metrics.iter().filter(|metric| metric.name == "pi.durable.tasks.active").map(|metric| metric.value).sum();
    assert_eq!(active, 0.0);
    assert!(metrics.iter().all(|metric| metric.attributes.iter().all(|(key, _)| !key.contains("id"))));
    assert!(setup.reports().is_empty(), "{:?}", setup.reports());
}

#[tokio::test]
async fn starts_a_shared_run_in_the_first_queued_inputs_trace_and_links_the_others() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    setup.settings.lock().unwrap().follow_up_mode = Some(QueueMode::All);
    let telemetry = InMemoryTelemetryContext::new();
    let (gate, reached) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let (opened, entered) = (gate.clone(), reached.clone());
    let held = FauxResponseStep::async_factory(move |_, _, _, _| {
        let (opened, entered) = (opened.clone(), entered.clone());
        Box::pin(async move {
            entered.notify_one();
            opened.notified().await;
            Ok(faux_assistant_message(vec![faux_text("first")], FauxMessageOptions::default()))
        })
    });
    setup.faux.set_responses(vec![held, answer("second")]);
    let (harness, root) = open_chat_with(Arc::new(MemoryStorage::new()), &setup, tracked(&setup, &telemetry)).await;
    let first = root.submit(SubmissionDraft::input("one"), &ctx()).await.unwrap();
    reached.notified().await;
    let second = root.submit(SubmissionDraft::input("two"), &ctx()).await.unwrap();
    let third = root.submit(SubmissionDraft::input("three"), &ctx()).await.unwrap();
    gate.notify_one();
    for submission in [&first, &second, &third] {
        let settled = submission.wait(&ctx()).await.unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done, "{settled:?} {:?}", setup.reports());
    }
    harness.wait_for_idle(&ctx()).await.unwrap();
    settled(&telemetry).await;

    let spans = submits(&telemetry);
    assert_eq!(spans.len(), 3);
    assert!(spans.iter().all(|span| span.parent_id.is_none()));
    let results: Vec<_> = spans.iter().map(|span| text(span, "result").unwrap()).collect();
    assert_eq!(results, ["placed", "queued", "queued"]);
    let traces: Vec<TelemetryCarrier> = spans.iter().map(|span| span.context.clone()).collect();
    let generations: Vec<TaskRecord> =
        conversation_tasks(&harness, &root).await.into_iter().filter(|task| task.kind == "pi.generation").collect();
    assert_eq!(generations.len(), 2);
    assert_eq!(generations[0].trace.as_ref(), Some(&traces[0]));
    assert_eq!(generations[0].links, None);
    assert_eq!(generations[1].trace.as_ref(), Some(&traces[1]));
    assert_eq!(generations[1].links.as_deref(), Some(&traces[2..]));
    let run = generations[1].id.get();
    let shared: Vec<_> = phases(&telemetry)
        .into_iter()
        .filter(|span| attribute(span, "task.id") == Some(&AttributeValue::from(run)))
        .collect();
    assert!(!shared.is_empty());
    for span in &shared {
        assert_eq!(span.parent_context.as_ref(), Some(&traces[1]));
        let links: Vec<_> = span.links.iter().map(|link| &link.context).collect();
        assert_eq!(links, [&traces[2]]);
    }
    harness.close(&ctx()).await.unwrap();
}

/// Sleeps far into the future in `start`, then completes in `after`.
fn nap() -> Task {
    define_task(
        TaskDefinition::new("test.nap", 1, |_| Ok(Json::from(json!({ "phase": "start" }))))
            .phase("start", |_, runtime, context| async move {
                let checkpoint = Json::from(json!({ "phase": "after", "attempt": 2 }));
                runtime.sleep_until(runtime.now() + 60_000.0, checkpoint, &context).await
            })
            .phase("after", |_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(completed(json!(null))) }, &context).await
            }),
    )
}

#[tokio::test]
async fn marks_only_the_first_invocation_after_reopen_resumed_and_keeps_its_stored_parent() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    let telemetry = InMemoryTelemetryContext::new();
    let now = Arc::new(std::sync::Mutex::new(1_000.0));
    let reopen = || async {
        let (storage, time, adapter) = (open_sqlite_storage(&path).await.unwrap(), now.clone(), telemetry.clone());
        let configure = move |options: HarnessOptions| {
            options.with_now(move || *time.lock().unwrap()).with_on_wake(|_| Ok(())).with_telemetry(Arc::new(adapter))
        };
        open_tasks_with(Arc::new(storage), vec![nap()], Arc::new(create_registry()), configure).await
    };
    let opened = reopen().await;
    let caller = telemetry.open_span(SpanOptions::new("caller"));
    let caller_context = telemetry.get_spans()[0].context.clone();
    let root = opened.harness.root(&ctx(), CreateOptions::default()).await.unwrap();
    let id = root.id;
    let create = move |tx: Tx| async move { tx.create_task(nap(), Json::Null, TaskOptions::conversation(id)).await };
    let task = root.commit(create, &with_telemetry(caller.clone(), &ctx())).await.unwrap();
    caller.end(SpanOutcome::Ok);
    opened.harness.resume().unwrap();
    wait_for(|| async {
        let record = opened.harness.get_task(task, &ctx()).await.unwrap().unwrap();
        record.status() == TaskStatus::Sleeping
    })
    .await;
    opened.harness.close(&ctx()).await.unwrap();

    *now.lock().unwrap() = 61_000.0;
    let opened = reopen().await;
    opened.harness.resume().unwrap();
    let record = opened.harness.wait_for_task(task, &ctx()).await.unwrap();
    assert_eq!(record.trace.as_ref(), Some(&caller_context));
    settled(&telemetry).await;
    let phases = phases(&telemetry);
    let summary: Vec<_> = phases
        .iter()
        .map(|span| (text(span, "phase").unwrap(), attribute(span, "resumed").cloned(), text(span, "outcome").unwrap()))
        .collect();
    assert_eq!(
        summary,
        [
            ("start".into(), Some(AttributeValue::Boolean(false)), "sleeping".into()),
            ("after".into(), Some(AttributeValue::Boolean(true)), "done".into()),
        ]
    );
    assert!(phases.iter().all(|span| span.parent_context.as_ref() == Some(&caller_context)));
    assert_eq!(attribute(&phases[1], "attempt"), Some(&AttributeValue::Number(2.0)));
    assert!(opened.reports.lock().unwrap().is_empty());
    opened.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn inherits_the_creators_trace_and_stores_no_carrier_for_an_explicit_root() {
    let telemetry = InMemoryTelemetryContext::new();
    let adapter = telemetry.clone();
    let configure = move |options: HarnessOptions| options.with_telemetry(Arc::new(adapter));
    let opened = open_tasks_with(Arc::new(MemoryStorage::new()), vec![], Arc::new(create_registry()), configure).await;
    let caller = telemetry.open_span(SpanOptions::new("caller"));
    let caller_context = telemetry.get_spans()[0].context.clone();
    let custom = TelemetryCarrier::from([("custom".to_owned(), "carrier".to_owned())]);
    let root = opened.harness.root(&ctx(), CreateOptions::default()).await.unwrap();
    let (id, carrier) = (root.id, custom.clone());
    let create = move |tx: Tx| async move {
        let mut ids = Vec::new();
        for trace in [TaskTrace::Inherit, TaskTrace::Root, TaskTrace::Carrier(carrier)] {
            let options = TaskOptions { trace, ..TaskOptions::conversation(id) };
            ids.push(tx.create_task(nap(), Json::Null, options).await?);
        }
        Ok(ids)
    };
    let ids = root.commit(create, &with_telemetry(caller, &ctx())).await.unwrap();
    let mut traces = Vec::new();
    for id in ids {
        traces.push(opened.harness.get_task(id, &ctx()).await.unwrap().unwrap().trace);
    }
    assert_eq!(traces, [Some(caller_context), None, Some(custom)]);
    opened.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn mirrors_reports_as_fixed_field_phase_events_and_type_only_counters() {
    let telemetry = InMemoryTelemetryContext::new();
    let reporter =
        define_task(TaskDefinition::new("test.reporter", 1, |_| Ok(Json::from(json!({ "phase": "start" })))).phase(
            "start",
            |_, runtime, context| async move {
                runtime.report(&DurableError::error("private-report"));
                runtime.commit(|_, _| async { Ok(completed(json!(null))) }, &context).await
            },
        ));
    let adapter = telemetry.clone();
    let configure = move |options: HarnessOptions| options.with_telemetry(Arc::new(adapter));
    let registry = Arc::new(create_registry());
    let opened = open_tasks_with(Arc::new(MemoryStorage::new()), vec![reporter.clone()], registry, configure).await;
    let root = opened.harness.root(&ctx(), CreateOptions::default()).await.unwrap();
    let id = root.id;
    let create = move |tx: Tx| async move { tx.create_task(reporter, Json::Null, TaskOptions::conversation(id)).await };
    let task = root.commit(create, &ctx()).await.unwrap();
    opened.harness.resume().unwrap();
    opened.harness.wait_for_task(task, &ctx()).await.unwrap();
    settled(&telemetry).await;

    let phase = phases(&telemetry).remove(0);
    let event = json!({ "type": "task_error", "extension": "tasks", "task.id": task.get() });
    assert!(has_event(&phase, "pi.durable.report", event), "{:?}", phase.events);
    assert_schema(&telemetry);
    assert!(!serde_json::to_string(&telemetry.get_spans()).unwrap().contains("private-report"));
    let reports: Vec<_> = telemetry.get_metrics().into_iter().filter(|m| m.name == "pi.durable.reports").collect();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].attributes, SpanAttributes::new().with("type", "task_error"));
    assert_eq!(opened.infos().len(), 1);
    opened.harness.close(&ctx()).await.unwrap();
}
