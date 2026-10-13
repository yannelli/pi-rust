//! Upstream `session-events` (event translation and extension UI) and `session-diff` tests.

use std::path::Path;

use agent_client_protocol_schema::{
    Error, RequestPermissionOutcome, RequestPermissionResponse, SelectedPermissionOutcome,
};
use serde_json::{Value, json};

use super::{FakeBackend, FakeClient, session_with, settle};
use crate::acp::session::AcpSession;
use std::sync::Arc;

struct Fixture {
    client: Arc<FakeClient>,
    backend: Arc<FakeBackend>,
    session: Arc<AcpSession>,
}

fn fixture(cwd: &Path) -> Fixture {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    let session = session_with(&backend, &client, cwd);
    Fixture { client, backend, session }
}

fn here() -> std::path::PathBuf {
    std::env::current_dir().unwrap()
}

impl Fixture {
    async fn emit(&self, event: Value) {
        self.session.handle_pi_event(event).await;
    }
}

#[tokio::test]
async fn text_and_thinking_deltas_become_chunks() {
    let f = fixture(&here());
    f.emit(json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "hi"}})).await;
    f.emit(json!({"type": "message_update", "assistantMessageEvent": {"type": "thinking_delta", "delta": "hmm"}}))
        .await;
    assert_eq!(
        f.client.updates(),
        [
            json!({"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}}}),
            json!({"sessionId": "s1", "update": {"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}}}),
        ]
    );
}

#[tokio::test]
async fn bash_tool_calls_render_as_terminals() {
    let f = fixture(&here());
    let cwd = here().display().to_string();
    f.emit(json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "bash", "args": {"command": "ls"}}))
        .await;
    f.emit(json!({"type": "tool_execution_update", "toolCallId": "t1",
        "partialResult": {"content": [{"type": "text", "text": "running"}]}}))
        .await;
    f.emit(json!({"type": "tool_execution_end", "toolCallId": "t1", "isError": false,
        "result": {"content": [{"type": "text", "text": "running done"}]}}))
        .await;
    let updates = f.client.update_bodies();
    assert_eq!(
        updates[0],
        json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "ls", "kind": "execute",
            "status": "in_progress", "content": [{"type": "terminal", "terminalId": "t1"}],
            "_meta": {"terminal_info": {"terminal_id": "t1", "cwd": cwd}}})
    );
    assert_eq!(
        updates[1],
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "in_progress",
            "_meta": {"terminal_output": {"terminal_id": "t1", "data": "running"}}})
    );
    assert_eq!(
        updates[2],
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
            "_meta": {"terminal_output": {"terminal_id": "t1", "data": " done"},
                "terminal_exit": {"terminal_id": "t1", "exit_code": 0, "signal": null}}})
    );
    assert_eq!(updates.len(), 3);
}

#[tokio::test]
async fn tool_calls_carry_locations_resolved_against_the_cwd() {
    let f = fixture(&here());
    f.emit(
        json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "read", "args": {"path": "src/a.rs"}}),
    )
    .await;
    f.emit(json!({"type": "tool_execution_update", "toolCallId": "t1",
        "partialResult": {"content": [{"type": "text", "text": "partial"}]}}))
        .await;
    f.emit(json!({"type": "tool_execution_end", "toolCallId": "t1", "isError": true,
        "result": {"content": [{"type": "text", "text": "missing"}]}}))
        .await;
    let updates = f.client.update_bodies();
    let path = here().join("src/a.rs").display().to_string();
    assert_eq!(updates[0]["locations"], json!([{"path": path}]));
    assert_eq!(
        (updates[0]["kind"].clone(), updates[0]["rawInput"].clone()),
        (json!("read"), json!({"path": "src/a.rs"}))
    );
    assert_eq!(updates[1]["content"], json!([{"type": "content", "content": {"type": "text", "text": "partial"}}]));
    assert_eq!(updates[1]["rawOutput"]["content"][0]["text"], "partial");
    assert_eq!(
        (updates[2]["status"].clone(), updates[2]["rawOutput"]["content"][0]["text"].clone()),
        (json!("failed"), json!("missing"))
    );
}

#[tokio::test]
async fn streamed_tool_calls_appear_before_execution_and_never_downgrade() {
    let f = fixture(&here());
    let start = json!({"type": "message_update", "assistantMessageEvent": {"type": "toolcall_start",
        "toolCall": {"id": "t1", "name": "write", "arguments": {"path": "/tmp/test.txt", "content": "hello"}}}});
    f.emit(start).await;
    f.emit(json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "write", "args": {"path": "/tmp/test.txt"}}))
        .await;
    let late = json!({"type": "message_update", "assistantMessageEvent": {"type": "toolcall_end", "contentIndex": 0,
        "partial": {"content": [{"type": "toolCall", "id": "t1", "name": "write", "partialArgs": "{\"path\":"}]}}});
    f.emit(late).await;
    let updates = f.client.update_bodies();
    assert_eq!(updates[0]["sessionUpdate"], "tool_call");
    assert!(updates[0].get("status").is_none(), "pending is the schema default and left out");
    assert_eq!(updates[0]["locations"], json!([{"path": "/tmp/test.txt"}]));
    assert_eq!(
        (updates[1]["sessionUpdate"].clone(), updates[1]["status"].clone()),
        (json!("tool_call_update"), json!("in_progress"))
    );
    assert_eq!(updates[2]["status"], "in_progress", "late deltas keep in_progress");
    assert_eq!(updates[2]["rawInput"], json!({"partialArgs": "{\"path\":"}));
}

#[tokio::test]
async fn edit_locations_carry_the_line_of_a_unique_old_text() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "one\ntwo\nneedle\nthree\n").unwrap();
    let path = file.display().to_string();
    let args = [
        json!({"path": "a.txt", "oldText": "needle"}),
        json!({"path": "a.txt", "edits": [{"oldText": "needle", "newText": "replacement"}]}),
        json!({"path": "a.txt", "edits": "[{\"oldText\":\"needle\",\"newText\":\"replacement\"}]"}),
    ];
    for args in args {
        let f = fixture(dir.path());
        f.emit(json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "edit", "args": args})).await;
        assert_eq!(f.client.update_bodies()[0]["locations"], json!([{"path": path, "line": 3}]));
    }
    std::fs::write(&file, "one\nneedle\ntwo\nneedle\n").unwrap();
    let f = fixture(dir.path());
    f.emit(json!({"type": "tool_execution_start", "toolCallId": "t2", "toolName": "edit",
        "args": {"path": "a.txt", "oldText": "needle"}}))
        .await;
    assert_eq!(f.client.update_bodies()[0]["locations"], json!([{"path": path}]));
}

#[tokio::test]
async fn retry_and_compaction_notices_keep_their_order() {
    let f = fixture(&here());
    f.emit(json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "before "}}))
        .await;
    f.emit(json!({"type": "auto_retry_start", "attempt": 2, "maxAttempts": 5, "delayMs": 2400,
        "errorMessage": "provider overloaded: 529"}))
        .await;
    f.emit(json!({"type": "auto_retry_start", "attempt": 1, "maxAttempts": 3, "delayMs": 1})).await;
    f.emit(json!({"type": "auto_retry_start", "attempt": "oops", "maxAttempts": null, "delayMs": "bad"})).await;
    f.emit(json!({"type": "auto_retry_end"})).await;
    f.emit(json!({"type": "auto_compaction_start"})).await;
    f.emit(json!({"type": "auto_compaction_end"})).await;
    f.emit(json!({"type": "compaction_start", "reason": "threshold"})).await;
    f.emit(json!({"type": "compaction_end", "reason": "threshold", "aborted": false, "willRetry": false})).await;
    f.emit(json!({"type": "compaction_start", "reason": "manual"})).await;
    let texts: Vec<String> =
        f.client.update_bodies().iter().map(|update| update["content"]["text"].as_str().unwrap().to_string()).collect();
    let start = "Context nearing limit, running automatic compaction...";
    let end = "Automatic compaction finished; context was summarized to continue the session.";
    assert_eq!(
        texts,
        [
            "before ",
            "Retrying (attempt 2/5, waiting 2s)...",
            "Retrying (attempt 1/3, waiting 1s)...",
            "Retrying...",
            "Retry finished, resuming.",
            start,
            end,
            start,
            end
        ]
    );
}

fn select(option: &str) -> Result<RequestPermissionResponse, Error> {
    Ok(RequestPermissionResponse::new(RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
        option.to_string(),
    ))))
}

#[tokio::test]
async fn extension_select_and_confirm_become_permission_requests() {
    let f = fixture(&here());
    f.client.answer(select("choice-1"));
    f.emit(json!({"type": "extension_ui_request", "id": "ui-1", "method": "select", "title": "Pick one",
        "options": ["Alpha", "Beta"]}))
        .await;
    settle().await;
    let request = serde_json::to_value(&f.client.permission_requests.lock().unwrap()[0]).unwrap();
    assert_eq!(
        request,
        json!({"sessionId": "s1", "toolCall": {"toolCallId": "pi-ui-ui-1", "title": "Pick one", "kind": "other",
            "status": "pending", "rawInput": {"method": "select", "title": "Pick one", "options": ["Alpha", "Beta"]}},
            "options": [{"optionId": "choice-0", "name": "Alpha", "kind": "allow_once"},
                {"optionId": "choice-1", "name": "Beta", "kind": "allow_once"}]})
    );
    f.client.answer(select("no"));
    f.emit(json!({"type": "extension_ui_request", "id": "ui-2", "method": "confirm", "title": "Clear session?",
        "message": "All messages will be lost."}))
        .await;
    settle().await;
    let confirm = serde_json::to_value(&f.client.permission_requests.lock().unwrap()[1]).unwrap();
    assert_eq!(
        confirm["options"],
        json!([{"optionId": "yes", "name": "Yes", "kind": "allow_once"}, {"optionId": "no", "name": "No", "kind": "reject_once"}])
    );
    f.client.answer(Ok(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)));
    f.emit(json!({"type": "extension_ui_request", "id": "ui-5", "method": "confirm", "title": "Continue?"})).await;
    settle().await;
    f.client.answer(Err(Error::internal_error()));
    f.emit(json!({"type": "extension_ui_request", "id": "ui-6", "method": "select", "options": ["x"]})).await;
    settle().await;
    assert_eq!(
        *f.backend.ui_responses.lock().unwrap(),
        [
            json!({"id": "ui-1", "value": "Beta"}),
            json!({"id": "ui-2", "confirmed": false}),
            json!({"id": "ui-5", "cancelled": true}),
            json!({"id": "ui-6", "cancelled": true}),
        ]
    );
}

#[tokio::test]
async fn unsupported_dialogs_are_cancelled_with_a_visible_note() {
    let f = fixture(&here());
    f.emit(json!({"type": "extension_ui_request", "id": "ui-3", "method": "input", "title": "Enter name"})).await;
    settle().await;
    f.emit(json!({"type": "extension_ui_request", "id": "ui-4", "method": "editor", "title": "Edit text"})).await;
    settle().await;
    f.emit(json!({"type": "extension_ui_request", "id": "ui-7", "method": "setStatus", "statusKey": "k"})).await;
    settle().await;
    assert_eq!(
        *f.backend.ui_responses.lock().unwrap(),
        [
            json!({"id": "ui-3", "cancelled": true}),
            json!({"id": "ui-4", "cancelled": true}),
            json!({"id": "ui-7", "cancelled": true})
        ]
    );
    let texts: Vec<Value> = f.client.update_bodies().iter().map(|update| update["content"]["text"].clone()).collect();
    assert_eq!(
        texts,
        [
            json!("Pi input UI request is not supported in ACP yet; cancelling it."),
            json!("Pi editor UI request is not supported in ACP yet; cancelling it.")
        ]
    );
}

#[tokio::test]
async fn notify_chunks_carry_their_level() {
    let f = fixture(&here());
    f.emit(json!({"type": "extension_ui_request", "id": "n1", "method": "notify", "message": "MCP: connection failed",
        "notifyType": "error"}))
        .await;
    settle().await;
    f.emit(json!({"type": "extension_ui_request", "id": "n2", "method": "notify", "message": "heads up"})).await;
    settle().await;
    assert_eq!(
        f.client.update_bodies(),
        [
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "MCP: connection failed"},
                "_meta": {"piAcp": {"notify": {"level": "error"}}}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "heads up"},
                "_meta": {"piAcp": {"notify": {"level": "info"}}}}),
        ]
    );
    assert_eq!(f.backend.ui_responses.lock().unwrap()[0], json!({"id": "n1", "cancelled": true}));
}

#[test]
fn choice_ids_must_be_canonical_indexes() {
    use crate::acp::option_index;
    assert_eq!(option_index("choice-1"), Some(1));
    assert_eq!([option_index("choice-01"), option_index("choice-"), option_index("yes")], [None, None, None]);
}

async fn completed_update(f: &Fixture) -> Value {
    f.client
        .update_bodies()
        .into_iter()
        .find(|update| update["sessionUpdate"] == "tool_call_update" && update["status"] == "completed")
        .expect("completed update")
}

#[tokio::test]
async fn edits_and_writes_report_structured_diffs_from_file_contents() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    let ok = json!({"content": [{"type": "text", "text": "ok"}]});
    for (tool, args) in [
        ("edit", json!({"path": "a.txt", "edits": [{"oldText": "before", "newText": "after"}]})),
        ("write", json!({"path": "a.txt", "content": "after\n"})),
    ] {
        std::fs::write(&file, "before\n").unwrap();
        let f = fixture(dir.path());
        f.emit(json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": tool, "args": args})).await;
        assert!(f.client.update_bodies()[0].get("content").is_none(), "no diff from requested args at start");
        f.emit(json!({"type": "tool_execution_update", "toolCallId": "t1", "partialResult": ok})).await;
        assert!(f.client.update_bodies()[1].get("rawOutput").is_none());
        std::fs::write(&file, "after\n").unwrap();
        f.emit(json!({"type": "tool_execution_end", "toolCallId": "t1", "isError": false, "result": ok})).await;
        let end = completed_update(&f).await;
        assert_eq!(
            end["content"],
            json!([{"type": "diff", "path": "a.txt", "oldText": "before\n", "newText": "after\n"}])
        );
        assert!(end.get("rawOutput").is_none(), "raw output is dropped with a diff");
    }
}

#[tokio::test]
async fn diffs_use_the_realized_contents_and_cover_new_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fuzzy.txt"), "FULLWIDTH: ＡＢＣ１２３\n").unwrap();
    let f = fixture(dir.path());
    f.emit(
        json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "edit", "args": {"path": "fuzzy.txt",
        "edits": [{"oldText": "FULLWIDTH: ABC123", "newText": "FULLWIDTH: ascii replacement"}]}}),
    )
    .await;
    std::fs::write(dir.path().join("fuzzy.txt"), "FULLWIDTH: ascii replacement\n").unwrap();
    f.emit(json!({"type": "tool_execution_end", "toolCallId": "t1", "isError": false,
        "result": {"content": [{"type": "text", "text": "Successfully replaced 1 block(s) in fuzzy.txt."}]}}))
        .await;
    let end = completed_update(&f).await;
    assert_eq!(end["content"][0]["oldText"], "FULLWIDTH: ＡＢＣ１２３\n");
    assert_eq!(end["content"][0]["newText"], "FULLWIDTH: ascii replacement\n");

    let f = fixture(dir.path());
    f.emit(json!({"type": "tool_execution_start", "toolCallId": "t2", "toolName": "write",
        "args": {"path": "new.txt", "content": "created\n"}}))
        .await;
    std::fs::write(dir.path().join("new.txt"), "created\n").unwrap();
    f.emit(json!({"type": "tool_execution_end", "toolCallId": "t2", "isError": false,
        "result": {"content": [{"type": "text", "text": "Successfully wrote 8 bytes to new.txt"}]}}))
        .await;
    let end = completed_update(&f).await;
    assert_eq!(end["content"], json!([{"type": "diff", "path": "new.txt", "newText": "created\n"}]));
}

#[tokio::test]
async fn failed_or_unchanged_mutations_fall_back_to_text() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "same\n").unwrap();
    let f = fixture(dir.path());
    f.emit(json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "edit", "args": {"path": "a.txt"}}))
        .await;
    f.emit(json!({"type": "tool_execution_end", "toolCallId": "t1", "isError": true,
        "result": {"content": [{"type": "text", "text": "No match"}]}}))
        .await;
    let end = f.client.update_bodies().pop().unwrap();
    assert_eq!(end["status"], "failed");
    assert_eq!(end["content"], json!([{"type": "content", "content": {"type": "text", "text": "No match"}}]));
}
