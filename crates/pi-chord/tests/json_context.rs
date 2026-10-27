//! Ports of upstream `test/json.test.ts` and `test/context.test.ts`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pi_chord::{
    BACKGROUND_CONTEXT, ChordError, CopyJsonOptions, TODO_CONTEXT, await_with_context, copy_json, create_context_key,
    is_json_value, json, trace_in, with_abort_signal, with_cancel, with_context_value, with_telemetry,
    without_abort_signal,
};
use pi_telemetry::{InMemoryTelemetryContext, SpanAttributes, SpanOptions, SpanOutcome, TelemetryContext};
use serde::Serialize;

#[derive(Serialize)]
struct Omitted {
    kept: i32,
    omitted: Option<i32>,
    nested: Nested,
}

#[derive(Serialize)]
struct Nested {
    omitted: Option<i32>,
    kept: bool,
}

#[test]
fn checks_strict_json_without_normalizing_it() {
    assert!(is_json_value(&json!({"nested": [1, true, null]})));
    assert!(!is_json_value(&BTreeMap::from([("omitted", None::<i32>)])));
    assert!(!is_json_value(&f64::INFINITY));
    assert!(!is_json_value(&vec![f64::NAN]));
}

#[test]
fn copies_strict_json_without_keeping_aliases() {
    let shared = json!({"value": 1});
    let input = json!({"left": shared.clone(), "right": shared.clone()});
    let copied = copy_json(&input, CopyJsonOptions::default()).unwrap();
    assert_eq!(copied, input);
    assert!(!copied.same_ref(&input));
    assert!(!copied["left"].same_ref(&shared));
    assert!(!copied["left"].same_ref(&copied["right"]));
}

#[test]
fn optionally_omits_undefined_object_properties_without_normalizing_arrays() {
    let input = Omitted { kept: 1, omitted: None, nested: Nested { omitted: None, kept: true } };
    let strict = copy_json(&input, CopyJsonOptions::default()).unwrap_err();
    assert!(strict.to_string().contains("strict JSON"), "{strict}");
    let options = CopyJsonOptions { omit_undefined_properties: true };
    assert_eq!(copy_json(&input, options).unwrap(), json!({"kept": 1, "nested": {"kept": true}}));
    let array = copy_json(&vec![None::<i32>], options).unwrap_err();
    assert!(array.to_string().contains("strict JSON"), "{array}");
}

#[test]
fn keeps_own_proto_data_properties() {
    let input = json!({"__proto__": {"safe": true}});
    let copied = copy_json(&input, CopyJsonOptions::default()).unwrap();
    assert_eq!(copied["__proto__"], json!({"safe": true}));
    assert_eq!(copied.to_string(), r#"{"__proto__":{"safe":true}}"#);
}

#[test]
fn provides_distinct_empty_root_contexts() {
    let key = create_context_key::<String>("value");
    assert!(!TODO_CONTEXT.ptr_eq(&BACKGROUND_CONTEXT));
    assert!(TODO_CONTEXT.abort_signal().is_none());
    assert!(TODO_CONTEXT.value(&key).is_none());
    assert_eq!(BACKGROUND_CONTEXT.to_string(), "[Context BACKGROUND_CONTEXT]");
    assert_eq!(TODO_CONTEXT.to_string(), "[Context TODO_CONTEXT]");
}

#[test]
fn layers_typed_values_without_changing_parents() {
    let first_key = create_context_key::<String>("first");
    let second_key = create_context_key::<i32>("second");
    let first = with_context_value(&first_key, "one".to_string(), &BACKGROUND_CONTEXT);
    let second = with_context_value(&second_key, 2, &first);
    let replaced = with_context_value(&first_key, "updated".to_string(), &second);
    assert!(BACKGROUND_CONTEXT.value(&first_key).is_none());
    assert_eq!(first.value(&first_key).map(String::as_str), Some("one"));
    assert!(first.value(&second_key).is_none());
    assert_eq!(second.value(&first_key).map(String::as_str), Some("one"));
    assert_eq!(second.value(&second_key), Some(&2));
    assert_eq!(replaced.value(&first_key).map(String::as_str), Some("updated"));
    assert_eq!(second.value(&first_key).map(String::as_str), Some("one"));
    assert_eq!(
        replaced.to_string(),
        "[Context BACKGROUND_CONTEXT].WithValue(first).WithValue(second).WithValue(first)"
    );
}

#[test]
fn inherits_parent_cancellation_and_isolates_child_cancellation() {
    let parent_controller = pi_chord::AbortController::new();
    let parent = with_abort_signal(&parent_controller.signal(), &BACKGROUND_CONTEXT);
    let (child, child_cancel) = with_cancel(&parent);
    let (sibling, _sibling_cancel) = with_cancel(&parent);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    child.abort_signal().unwrap().add_listener(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    child_cancel.abort(Some(ChordError::error("child")));
    assert!(child.abort_signal().unwrap().aborted());
    assert_eq!(child.abort_signal().unwrap().reason().unwrap().to_string(), "child");
    assert!(!sibling.abort_signal().unwrap().aborted());
    assert!(!parent.abort_signal().unwrap().aborted());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    parent_controller.abort(Some(ChordError::error("parent")));
    assert!(sibling.abort_signal().unwrap().aborted());
    assert_eq!(sibling.abort_signal().unwrap().reason().unwrap().to_string(), "parent");
}

#[test]
fn masks_caller_cancellation_for_mandatory_cleanup() {
    let controller = pi_chord::AbortController::new();
    let key = create_context_key::<String>("value");
    let context = with_context_value(
        &key,
        "preserved".to_string(),
        &with_abort_signal(&controller.signal(), &BACKGROUND_CONTEXT),
    );
    let cleanup = without_abort_signal(&context);
    controller.abort(None);
    assert!(context.abort_signal().unwrap().aborted());
    assert!(cleanup.abort_signal().is_none());
    assert_eq!(cleanup.value(&key).map(String::as_str), Some("preserved"));
}

#[tokio::test]
async fn stops_waiting_when_the_invocation_is_cancelled() {
    let controller = pi_chord::AbortController::new();
    let context = with_abort_signal(&controller.signal(), &BACKGROUND_CONTEXT);
    let (send, receive) = tokio::sync::oneshot::channel::<&str>();
    let work = async move { receive.await.map_err(|_| ChordError::error("dropped")) };
    let waiting = tokio::spawn(async move { await_with_context(work, &context).await });
    tokio::task::yield_now().await;
    controller.abort(Some(ChordError::error("cancelled")));
    assert_eq!(waiting.await.unwrap().unwrap_err().to_string(), "cancelled");
    assert!(send.send("completed later").is_err());
    let completed = await_with_context(async { Ok::<_, ChordError>("completed") }, &BACKGROUND_CONTEXT).await;
    assert_eq!(completed.unwrap(), "completed");
}

#[tokio::test]
async fn carries_an_explicit_telemetry_span_without_changing_cancellation() {
    let telemetry = InMemoryTelemetryContext::new();
    BACKGROUND_CONTEXT.telemetry().add_event("dropped", SpanAttributes::new());
    let (parent, controller) = with_cancel(&BACKGROUND_CONTEXT);
    let traced = trace_in(
        SpanOptions::new("ignored"),
        |context| async move {
            // The parent's NOOP span opens NOOP children, so nothing records.
            context.telemetry().add_event("dropped", SpanAttributes::new());
            Ok::<_, ChordError>(context)
        },
        &parent,
    );
    traced.await.unwrap();
    let span = telemetry.open_span(SpanOptions::new("outer"));
    let context = with_telemetry(span.clone(), &parent);
    let child = trace_in(
        SpanOptions::new("inner"),
        |child| async move {
            child.telemetry().add_event("inside", SpanAttributes::new());
            Ok::<_, ChordError>(child)
        },
        &context,
    )
    .await
    .unwrap();
    span.end(SpanOutcome::Ok);
    controller.abort(None);
    assert!(context.is_cancelled() && child.is_cancelled());
    assert!(Arc::ptr_eq(&context.telemetry(), &span));

    let spans = telemetry.get_spans();
    let names: Vec<_> = spans.iter().map(|span| (span.name.as_str(), span.parent_id)).collect();
    assert_eq!(names, [("outer", None), ("inner", Some(1))]);
    assert_eq!(spans[1].events.len(), 1);
}
