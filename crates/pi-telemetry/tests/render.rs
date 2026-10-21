//! Port of upstream `test/render.test.ts`. Attributes print in key order because `SpanAttributes` is sorted.

use pi_telemetry::render::render_trace;
use pi_telemetry::{SpanAttributes, SpanStatus, StoredEvent, StoredLink, StoredSpan};

fn span(id: &str) -> StoredSpan {
    StoredSpan {
        id: id.into(),
        trace_id: "trace".into(),
        parent_id: None,
        name: id.into(),
        started_ms: 0.0,
        ended_ms: 10.0,
        attributes: SpanAttributes::new(),
        events: Vec::new(),
        links: Vec::new(),
        status: SpanStatus::Ok,
    }
}

fn child(id: &str, parent: &str) -> StoredSpan {
    StoredSpan { parent_id: Some(parent.into()), ..span(id) }
}

fn link(trace_id: &str, span_id: &str, attributes: Option<SpanAttributes>) -> StoredLink {
    StoredLink { trace_id: trace_id.into(), span_id: span_id.into(), attributes }
}

fn event(name: &str, time_ms: f64, attributes: SpanAttributes) -> StoredEvent {
    StoredEvent { name: name.into(), time_ms, attributes }
}

#[test]
fn renders_an_unordered_trace_with_attributes_durations_errors_events_and_links() {
    let attributes = SpanAttributes::new()
        .with("request", "a b")
        .with("attempt", 2)
        .with("cached", false)
        .with("tags", vec!["a", "b"])
        .with("sizes", vec![1.0, 2.0])
        .with("flags", vec![true, false]);
    let root = StoredSpan {
        started_ms: 1000.0,
        ended_ms: 1020.0,
        attributes,
        events: vec![
            event("checkpoint", 2.0, SpanAttributes::new().with("phase", "tools")),
            event("tick", 3.5, SpanAttributes::new()),
        ],
        links: vec![
            link("remote-trace", "remote", Some(SpanAttributes::new().with("reason", "steer"))),
            link("other-trace", "other", None),
        ],
        ..span("root")
    };
    let failed = SpanStatus::error("Error", "failed call");
    let first = StoredSpan { started_ms: 1005.0, ended_ms: 1009.5, status: failed, ..child("first", "root") };
    let nested = StoredSpan { started_ms: 1006.0, ended_ms: 1007.0, ..child("nested", "first") };
    let status = SpanStatus::Error { error: None };
    let second = StoredSpan { started_ms: 1010.0, ended_ms: 1012.0, status, ..child("second", "root") };
    let spans = [second, nested, first, root];
    let expected = [
        r#"root attempt=2 cached=false flags=[true,false] request="a b" sizes=[1,2] tags=["a","b"] (20ms)"#,
        r#"  · checkpoint phase="tools" (+2ms)"#,
        "  · tick (+3.5ms)",
        r#"  → link trace remote-trace span remote reason="steer""#,
        "  → link trace other-trace span other",
        "  first (4.5ms) error Error: failed call",
        "    nested (1ms)",
        "  second (2ms) error",
    ];
    assert_eq!(render_trace(&spans), expected.join("\n"));
}

#[test]
fn treats_missing_parents_as_roots_and_orders_roots_and_children_by_start_time() {
    let later = StoredSpan { started_ms: 20.0, ended_ms: 30.0, ..child("later", "external") };
    let nested = StoredSpan { started_ms: 25.0, ended_ms: 30.0, ..child("child", "later") };
    let earlier =
        StoredSpan { started_ms: 10.0, ended_ms: 15.0, links: vec![link("trace", "later", None)], ..span("earlier") };
    let expected = ["earlier (5ms)", "  → link trace trace span later", "later (10ms)", "  child (5ms)"];
    assert_eq!(render_trace(&[later, nested, earlier]), expected.join("\n"));
}

#[test]
fn prints_cross_trace_links_without_following_them_including_cycles() {
    let server = StoredSpan {
        name: "server".into(),
        trace_id: "server-trace".into(),
        links: vec![link("worker-trace", "worker:1", None)],
        ..span("server:1")
    };
    let worker = StoredSpan {
        name: "worker".into(),
        trace_id: "worker-trace".into(),
        links: vec![link("server-trace", "server:1", None)],
        ..span("worker:1")
    };
    let expected = [
        "server (10ms)",
        "  → link trace worker-trace span worker:1",
        "worker (10ms)",
        "  → link trace server-trace span server:1",
    ];
    assert_eq!(render_trace(&[server, worker]), expected.join("\n"));
}

#[test]
fn deduplicates_spans_merged_from_stores_by_global_id() {
    let root = StoredSpan { name: "root".into(), ..span("server:1") };
    let done = event("done", 3.0, SpanAttributes::new());
    let middle = StoredSpan { name: "child".into(), events: vec![done], ..child("worker:1", "server:1") };
    let leaf = StoredSpan { name: "grandchild".into(), ..child("worker:2", "worker:1") };
    let spans = [middle.clone(), root.clone(), leaf, root, middle];
    let expected = "root (10ms)\n  child (10ms)\n    · done (+3ms)\n    grandchild (10ms)";
    assert_eq!(render_trace(&spans), expected);
}

#[test]
fn preserves_input_order_for_equal_start_times() {
    let spans = [span("root"), child("z", "root"), child("a", "root"), span("other")];
    assert_eq!(render_trace(&spans), "root (10ms)\n  z (10ms)\n  a (10ms)\nother (10ms)");
}

#[test]
fn escapes_string_attribute_values_without_hiding_their_contents() {
    let attributes = SpanAttributes::new().with("text", "line one\n\"line two\"");
    let spans = [StoredSpan { attributes, ..span("root") }];
    assert_eq!(render_trace(&spans), "root text=\"line one\\n\\\"line two\\\"\" (10ms)");
}

#[test]
fn renders_deep_trees_without_recursive_calls() {
    let spans: Vec<StoredSpan> = (0..2000)
        .map(|index| if index == 0 { span("0") } else { child(&index.to_string(), &(index - 1).to_string()) })
        .collect();
    let rendered = render_trace(&spans);
    assert_eq!(rendered.split('\n').count(), spans.len());
    assert!(rendered.ends_with(&format!("{}1999 (10ms)", "  ".repeat(1999))));
}

#[test]
fn renders_an_empty_input_as_an_empty_string() {
    assert_eq!(render_trace(&[]), "");
}
