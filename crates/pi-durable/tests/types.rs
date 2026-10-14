//! Port of upstream `test/types.test.ts`. Upstream checks TypeScript types with `expectTypeOf` and
//! `@ts-expect-error`; here the same values are built from the Rust types and checked against the upstream JSON.
//! Each rejected shape is listed with whether a Rust type rules it out.

use pi_chord::Json;
use pi_chord::delta::Op;
use pi_durable::harness::*;
use pi_durable::*;
use serde_json::{Value, json};

fn encoded<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap()
}

#[test]
fn brands_numeric_ids_by_record_kind_and_carries_task_result_types() {
    let conversation = ConversationId(1);
    let task = TaskId(4);
    assert_eq!(encoded(&conversation), json!(1));
    assert_eq!(serde_json::to_string(&task).unwrap(), "4");
    // Each ID is its own newtype, so mixing kinds or using an ID as a commit sequence fails to compile. Rust task
    // IDs carry no result type, so the typed narrowing upstream rejects has no counterpart.
    let _: (EntryId, DocumentId, SubmissionId, Seq) = (EntryId(2), DocumentId(6), SubmissionId(5), Seq(1));
}

#[test]
fn encodes_discriminator_dependent_fields() {
    let (entry, answer, submission, conversation) = (EntryId(2), EntryId(3), SubmissionId(5), ConversationId(1));
    assert_eq!(encoded(&ContextEdit::omit(entry)), json!({ "target": 2, "action": "omit" }));
    let replace = ContextEdit { target: entry, action: ContextEditAction::Replace, messages: Some(Vec::new()) };
    assert_eq!(encoded(&replace), json!({ "target": 2, "action": "replace", "messages": [] }));
    let pending = TaskState::Pending { checkpoint: Json::from(json!({ "phase": "ready" })) };
    assert_eq!(encoded(&pending), json!({ "status": "pending", "checkpoint": { "phase": "ready" } }));
    let terminal =
        TaskState::Terminal { outcome: TaskOutcome::Completed { result: Some(Json::from(json!({ "value": 1 }))) } };
    let expected = json!({ "status": "terminal", "outcome": { "status": "completed", "result": { "value": 1 } } });
    assert_eq!(encoded(&terminal), expected);
    let done = |kind: SubmissionType, answer: Option<EntryId>| SubmissionRecord {
        conversation_id: conversation,
        kind,
        request_id: None,
        status: SubmissionStatus::Done,
        entry: Some(entry),
        answer,
        reason: None,
        detail: None,
        id: submission,
    };
    let input = json!({ "conversationId": 1, "type": "input", "status": "done", "entry": 2, "answer": 3, "id": 5 });
    assert_eq!(encoded(&done(SubmissionType::Input, Some(answer))), input);
    let write = json!({ "conversationId": 1, "type": "write", "status": "done", "entry": 2, "id": 5 });
    assert_eq!(encoded(&done(SubmissionType::Write, None)), write);
    let queued = SubmissionCreate::queued(conversation, SubmissionType::Write, None);
    assert_eq!((queued.kind, queued.status), (SubmissionType::Write, SubmissionStatus::Queued));
    let base = DocumentContent::Base { version: 1, value: Json::from(json!({ "count": 1 })) };
    assert_eq!(encoded(&base), json!({ "kind": "base", "version": 1, "value": { "count": 1 } }));
    let ops: Vec<Op> = serde_json::from_value(json!([["s", ["count"], 2]])).unwrap();
    let delta = DocumentContent::Delta { version: 1, ops };
    assert_eq!(encoded(&delta), json!({ "kind": "delta", "version": 1, "ops": [["s", ["count"], 2]] }));
    let document = DocumentCreate {
        id: DocumentId(6),
        kind: "test".into(),
        key: None,
        scope: DocumentScope::Conversation { conversation_id: conversation },
        history: Some(DocumentHistory::Rewindable),
        fork: Some(DocumentFork::AsOf),
    };
    let created = json!({
        "id": 6, "kind": "test", "scope": { "kind": "conversation", "conversationId": 1 },
        "history": "rewindable", "fork": "asOf",
    });
    assert_eq!(encoded(&document), created);

    // Rust enums rule out a live state with an outcome, a terminal state with a checkpoint, a base with operations,
    // a delta with a value, a completed outcome with an error, and a create value with an ID or `createdAt`.
    // Edits, memos, document policies, and submission entries and answers are optional fields, checked at runtime.
}

/// Custom hooks of the stepper task.
struct Hooks {
    before_step: fn(u32) -> Option<bool>,
}

#[test]
fn types_submissions_task_waits_compaction_and_tasks_erased_into_extensions() {
    let input = SubmissionDraft::input("hi").with_when_busy(WhenBusy::Steer);
    assert!(matches!(input, SubmissionDraft::Input { when_busy: Some(WhenBusy::Steer), .. }));
    let write = SubmissionDraft::write(EntryDraft::new("note"));
    assert!(matches!(&write, SubmissionDraft::Write { entry, .. } if entry.kind == "note"));
    // A write draft has no content and no busy policy, and an input draft no entry: the enum rules them out.
    let stepper = define_task(
        TaskDefinition::new("test.stepper", 1, |input| {
            let steps = input.get("steps").cloned().unwrap_or(Json::Null).to_serde();
            Ok(Json::from(json!({ "phase": "plan", "steps": steps })))
        })
        .phase("plan", |_, runtime, context| async move {
            let next = Some(TaskState::Running { checkpoint: Json::from(json!({ "phase": "run", "step": 0 })) });
            runtime.commit(move |_, _| async move { Ok(next) }, &context).await
        })
        .phase("run", |_, runtime, context| async move {
            for hooks in runtime.hooks::<Hooks>(&context).await? {
                let _ = (hooks.before_step)(1);
            }
            let ran = Some(Json::from(json!({ "ran": 1 })));
            let done = Some(TaskState::Terminal { outcome: TaskOutcome::Completed { result: ran } });
            runtime.commit(move |_, _| async move { Ok(done) }, &context).await
        })
        .abort(|_, _, _| async { Ok(()) }),
    );
    let registration = hook(&stepper, Hooks { before_step: |step| (step > 1).then_some(true) });
    // Erased into an extension, whatever its input, phases, and hooks.
    let extension = Extension::new("stepper").task(stepper.clone()).hook(registration);
    assert_eq!(extension.name, "stepper");
    assert!(stepper.phase_handler("plan").is_some() && stepper.phase_handler("run").is_some());
    // Task input, checkpoints, waits, and compaction results are JSON in Rust, and hooks are found by type at
    // `runtime.hooks::<H>()`: a wrongly typed handler is skipped there instead of failing to compile.
}
