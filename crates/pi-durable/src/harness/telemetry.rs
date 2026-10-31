//! Durable's content-free span, event, and metric vocabulary (upstream `harness/telemetry.ts`). IDs address durable
//! records, never content.

use std::sync::{Arc, LazyLock};

use pi_chord::Context;
use pi_telemetry::{
    NoopTelemetryMetrics, SchemaTelemetryCounter, SchemaTelemetryHistogram, SpanAttributes, TelemetryContext,
    TelemetrySchemaDefinition, create_typed_metrics, define_telemetry_schema, noop_telemetry_context,
};
use serde_json::json;

use crate::types::ReportInfo;

pub static DURABLE_TELEMETRY_SCHEMA: LazyLock<TelemetrySchemaDefinition> = LazyLock::new(|| {
    let entry = json!({
        "description": "A transcript entry was committed; content remains in the transcript.",
        "attributes": {
            "entry.id": {"type": "number", "required": true, "description": "Transcript entry ID.", "cardinality": "high"},
            "entry.kind": {"type": "string", "required": true, "description": "Entry kind."},
        },
    });
    let report_type = json!({
        "type": "string",
        "required": true,
        "description": "ReportInfo type.",
        "values": [
            "session_failed", "task_faulted", "task_interrupted", "migration_failed", "task_outdated", "task_error",
            "extension_error", "host_error", "listener_error",
        ],
    });
    let report = json!({
        "description": "A report was raised; no error payload is recorded.",
        "attributes": {
            "type": report_type,
            "reason": {
                "type": "string",
                "required": false,
                "description": "Fixed report reason, when present.",
                "values": ["threw", "restart_limit", "interrupted", "missing_task", "incompatible_task"],
            },
            "extension": {"type": "string", "required": false, "description": "Registered extension name, when present."},
            "task.id": {"type": "number", "required": false, "description": "Task ID, when present.", "cardinality": "high"},
        },
    });
    let submission = json!({
        "description": "A submission was settled.",
        "attributes": {
            "submission.id": {"type": "number", "required": true, "description": "Submission ID.", "cardinality": "high"},
            "status": {"type": "string", "required": true, "values": ["done", "unanswered"], "description": "Settlement."},
            "answer.entry_id": {"type": "number", "required": false, "description": "Answer entry ID.", "cardinality": "high"},
        },
    });
    let schema = json!({
        "version": 1,
        "spans": {
            "pi.durable.submit": {
                "description": "Public submission admission and its commit. Inputs start root traces linked to their callers; writes are children. Internal admissions emit phase events only.",
                "parents": {"kind": "any"},
                "startAttributes": {
                    "submission.type": {"type": "string", "required": true, "values": ["input", "write"], "description": "Submission type."},
                    "conversation.id": {"type": "number", "required": true, "description": "Conversation ID.", "cardinality": "high"},
                },
                "endAttributes": {
                    "submission.id": {"type": "number", "description": "Admitted submission ID.", "cardinality": "high"},
                    "result": {"type": "string", "values": ["queued", "placed", "done"], "description": "Admission result; stale writes report done."},
                },
                "events": {"pi.durable.entry": entry, "pi.durable.submission": submission, "pi.durable.report": report},
                "status": {"default": "ok", "errorWhen": "Admission failed."},
            },
            "pi.durable.phase": {
                "description": "One task phase invocation, including its decision commit, never a wait or restart.",
                "parents": {"kind": "any"},
                "startAttributes": {
                    "task.id": {"type": "number", "required": true, "description": "Task ID.", "cardinality": "high"},
                    "task.name": {"type": "string", "required": true, "description": "Registered task name."},
                    "phase": {"type": "string", "required": true, "description": "Invoked checkpoint phase."},
                    "kind": {"type": "string", "required": true, "values": ["run", "abort"], "description": "Invocation kind."},
                    "attempt": {"type": "number", "required": false, "description": "Checkpoint attempt, when present."},
                    "resumed": {"type": "boolean", "required": true, "description": "First invocation of a task loaded in this process."},
                },
                "endAttributes": {
                    "outcome": {"type": "string", "description": "next=<phase>, waiting, sleeping, done, faulted, or aborted."},
                    "tool.name": {"type": "string", "description": "Called tool name."},
                    "tool.call_id": {"type": "string", "description": "Transcript tool call ID.", "cardinality": "high"},
                },
                "events": {
                    "pi.durable.entry": entry,
                    "pi.durable.submission": submission,
                    "pi.durable.report": report,
                    "pi.durable.checkpoint": {
                        "description": "The running checkpoint's phase changed while the invocation continued.",
                        "attributes": {"phase": {"type": "string", "required": true, "description": "Committed checkpoint phase."}},
                    },
                    "pi.durable.replay": {
                        "description": "A nested tool call was answered from its stored result.",
                        "attributes": {"tool.name": {"type": "string", "required": true, "description": "Called tool name."}},
                    },
                },
                "status": {
                    "default": "ok",
                    "errorWhen": "Faulted, failed, and orphaned outcomes use task_faulted, task_failed, and task_orphaned with a diagnostic capped at 500 characters. Aborted outcomes keep status ok.",
                },
            },
        },
        "metrics": {
            "pi.durable.reports": {
                "kind": "counter",
                "description": "Reports raised, grouped by their fixed ReportInfo type.",
                "attributes": {"type": report_type},
            },
            "pi.durable.phase.duration": {
                "kind": "histogram",
                "unit": "ms",
                "description": "Duration of a phase invocation and its decision commit.",
                "attributes": {
                    "task": {"type": "string", "required": true, "description": "Registered task name."},
                    "phase": {"type": "string", "required": true, "description": "Invoked checkpoint phase."},
                    "outcome": {"type": "string", "required": true, "description": "Phase outcome."},
                },
            },
            "pi.durable.tasks.active": {
                "kind": "upDownCounter",
                "description": "Live tasks held by this Harness; decremented at terminal settlement or close.",
                "attributes": {"task": {"type": "string", "required": true, "description": "Registered task name."}},
            },
        },
    });
    define_telemetry_schema(serde_json::from_value(schema).expect("the durable telemetry schema is well formed"))
});

/// The host's adapter and Durable's instruments over it; no-ops without `HarnessOptions::telemetry`.
pub(crate) struct DurableTelemetry {
    pub root: Arc<dyn TelemetryContext>,
    reports: SchemaTelemetryCounter,
    pub active: SchemaTelemetryCounter,
    pub duration: SchemaTelemetryHistogram,
}

impl DurableTelemetry {
    pub fn new(root: Option<Arc<dyn TelemetryContext>>) -> Self {
        let root = root.unwrap_or_else(noop_telemetry_context);
        let metrics = root.metrics().unwrap_or_else(|| Arc::new(NoopTelemetryMetrics));
        let schema = std::slice::from_ref(&*DURABLE_TELEMETRY_SCHEMA);
        let metrics = create_typed_metrics(metrics, schema).expect("the durable schema has unique metric names");
        let declared = "the durable schema declares its metrics";
        Self {
            reports: metrics.counter("pi.durable.reports").expect(declared),
            active: metrics.up_down_counter("pi.durable.tasks.active").expect(declared),
            duration: metrics.histogram("pi.durable.phase.duration").expect(declared),
            root,
        }
    }

    /// Count a report by type and add a fixed-field `pi.durable.report` event to the context's span; no error payload.
    pub fn report(&self, info: &ReportInfo, context: Option<&Context>) {
        let info = serde_json::to_value(info).unwrap_or_default();
        let kind = info["type"].as_str().unwrap_or_default().to_owned();
        let _ = self.reports.add(1.0, SpanAttributes::new().with("type", kind.clone()));
        let Some(context) = context else { return };
        let attributes = SpanAttributes::new()
            .with("type", kind)
            .with_optional("reason", info["reason"].as_str())
            .with_optional("extension", info["extension"].as_str())
            .with_optional("task.id", info["task"]["id"].as_f64());
        context.telemetry().add_event("pi.durable.report", attributes);
    }
}
