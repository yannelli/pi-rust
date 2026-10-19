//! The remaining upstream context derivation cases: system messages, tool results, forks, and reads as of an entry.

use pi_ai::{SystemMessage, ToolCall, ToolResultMessage};
use pi_durable::ConversationOwnership;

use super::*;

/// An assistant message with tool calls `tool-<id>` (upstream `assistant(text, { calls })`).
pub fn calling(text: &str, calls: &[&str], stop_reason: StopReason) -> Message {
    let mut content = vec![ContentBlock::text(text)];
    for id in calls {
        let call = ToolCall {
            id: (*id).to_owned(),
            name: format!("tool-{id}"),
            arguments: Default::default(),
            ..ToolCall::default()
        };
        content.push(ContentBlock::ToolCall(call));
    }
    Message::Assistant(faux_assistant_message(content, FauxMessageOptions::stop_reason(stop_reason)))
}

pub fn tool_result(id: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.to_owned(),
        tool_name: format!("tool-{id}"),
        content: vec![ContentBlock::text(format!("result {id}"))],
        details: None,
        usage: None,
        nested_calls: None,
        duration_ms: None,
        is_error: false,
        timestamp: 3,
    })
}

pub fn system(key: &str, text: &str) -> Message {
    let sections = serde_json::Map::from_iter([(key.to_owned(), json!(text))]);
    Message::System(SystemMessage {
        content: UserContent::Text(String::new()),
        sections: Some(sections),
        tools_added: None,
        tools_removed: None,
        timestamp: 4,
    })
}

impl Setup {
    async fn kind(&self, message: Message, kind: &str) -> EntryRecord {
        self.append(EntryDraft { model: Some(vec![message]), ..EntryDraft::new(kind) }).await
    }
}

fn ownerless() -> CreateOptions {
    CreateOptions { ownership: ConversationOwnership::Ownerless, ..CreateOptions::default() }
}

#[tokio::test]
async fn keeps_positional_system_messages_and_orders_tool_results_by_call_order() {
    let setup = Setup::new().await;
    setup.kind(system("preamble", "You help."), "pi.system").await;
    setup.message(user("run tools")).await;
    setup.message(calling("calling", &["b", "a"], StopReason::Stop)).await;
    setup.message(tool_result("a")).await;
    setup.kind(system("cwd", "/repo"), "pi.system").await;
    setup.message(tool_result("b")).await;
    setup.message(tool_result("zz")).await;
    setup.message(assistant("done", StopReason::Stop)).await;
    assert_eq!(
        described(&setup.view().await),
        [
            "system:preamble",
            "user:run tools",
            "assistant:calling",
            "result:b:result b",
            "result:a:result a",
            "system:cwd",
            "assistant:done",
        ]
    );
    setup.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn leads_with_a_system_message_that_only_user_messages_precede_and_keeps_later_ones_in_place() {
    let setup = Setup::new().await;
    setup.message(user("first")).await;
    setup.message(user("steered")).await;
    setup.kind(system("preamble", "You help."), "pi.system").await;
    setup.message(assistant("answer", StopReason::Stop)).await;
    setup.message(user("next")).await;
    setup.kind(system("cwd", "/repo"), "pi.system").await;
    let view = setup.view().await;
    assert_eq!(
        described(&view),
        ["system:preamble", "user:first", "user:steered", "assistant:answer", "user:next", "system:cwd"]
    );
    // Stored order and contributions stay as committed.
    let stored: Vec<String> = view.contributions.iter().flatten().map(describe).take(3).collect();
    assert_eq!(stored, ["user:first", "user:steered", "system:preamble"]);

    // After a reset, the baseline written after the handoff leads too.
    let reset =
        EntryDraft { head: Some(DraftHead::SelfEntry), model: Some(vec![user("handoff")]), ..EntryDraft::new("reset") };
    setup.append(reset).await;
    setup.kind(system("preamble", "Baseline."), "pi.system").await;
    assert_eq!(described(&setup.view().await), ["system:preamble", "user:handoff"]);
    setup.harness.close(&context()).await.unwrap();
}

fn summary(head: EntryId) -> EntryDraft {
    EntryDraft { head: Some(DraftHead::Entry(head)), model: Some(vec![user("summary")]), ..EntryDraft::new("summary") }
}

fn edit(target: EntryId, messages: Option<Vec<Message>>) -> EntryDraft {
    let edit = match messages {
        Some(messages) => ContextEdit { target, action: ContextEditAction::Replace, messages: Some(messages) },
        None => ContextEdit::omit(target),
    };
    EntryDraft { edits: Some(vec![edit]), ..EntryDraft::new("edit") }
}

#[tokio::test]
async fn leads_with_the_baseline_after_a_compaction_that_keeps_assistant_messages() {
    let setup = Setup::new().await;
    setup.message(user("old")).await;
    let kept = setup.message(user("kept")).await;
    setup.message(calling("calling", &["a"], StopReason::Stop)).await;
    setup.message(tool_result("a")).await;
    setup.message(assistant("answer", StopReason::Stop)).await;
    setup.append(summary(kept.id)).await;
    setup.message(user("next")).await;
    setup.kind(system("preamble", "Baseline."), "pi.system").await;
    let expected =
        ["system:preamble", "user:summary", "user:kept", "assistant:calling", "result:a:result a", "assistant:answer"];
    assert_eq!(described(&setup.view().await)[..6], expected);

    // Later system messages stay in place.
    setup.message(assistant("done", StopReason::Stop)).await;
    setup.kind(system("cwd", "/repo"), "pi.system").await;
    let view = described(&setup.view().await);
    assert_eq!(view[..2], expected[..2]);
    assert_eq!(view[view.len() - 2..], ["assistant:done", "system:cwd"]);

    // Without a head marker, a baseline written after the first answers leads too.
    let fresh = Setup::new().await;
    fresh.message(user("first")).await;
    fresh.message(assistant("answer", StopReason::Stop)).await;
    fresh.kind(system("plan", "Plan."), "pi.system").await;
    assert_eq!(described(&fresh.view().await), ["system:plan", "user:first", "assistant:answer"]);
    setup.harness.close(&context()).await.unwrap();
    fresh.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn only_the_baseline_moves_past_assistant_messages() {
    // Another kind's system message is positional, so the baseline after it is not the first system message.
    let setup = Setup::new().await;
    setup.message(user("first")).await;
    setup.message(assistant("answer", StopReason::Stop)).await;
    setup.kind(system("note", "From here on."), "host.note").await;
    setup.kind(system("preamble", "Baseline."), "pi.system").await;
    assert_eq!(described(&setup.view().await), ["user:first", "assistant:answer", "system:note", "system:preamble"]);

    // A first pi.system entry written after another system message is no baseline, even once that is omitted.
    let other = Setup::new().await;
    let host = other.kind(system("legacy", "Use the legacy format."), "host.system").await;
    other.message(user("first")).await;
    other.message(assistant("answer", StopReason::Stop)).await;
    other.kind(system("legacy", ""), "pi.system").await;
    other.append(edit(host.id, None)).await;
    assert_eq!(described(&other.view().await), ["user:first", "assistant:answer", "system:legacy"]);

    // A retained system entry stays in place until the baseline after the head marker leads.
    let third = Setup::new().await;
    let kept = third.message(user("kept")).await;
    third.message(assistant("answer", StopReason::Stop)).await;
    let retained = third.kind(system("cwd", "/repo"), "pi.system").await;
    third.append(summary(kept.id)).await;
    third.message(user("next")).await;
    let before = ["user:summary", "user:kept", "assistant:answer", "system:cwd", "user:next"];
    assert_eq!(described(&third.view().await), before);
    let baseline = EntryDraft {
        model: Some(vec![system("preamble", "Baseline.")]),
        edits: Some(vec![ContextEdit::omit(retained.id)]),
        ..EntryDraft::new("pi.system")
    };
    let baseline = third.append(baseline).await;
    third.message(assistant("done", StopReason::Stop)).await;
    assert_eq!(
        described(&third.view().await),
        ["system:preamble", "user:summary", "user:kept", "assistant:answer", "user:next", "assistant:done"]
    );

    // A replaced baseline is not one.
    third.append(edit(baseline.id, Some(vec![system("other", "Other.")]))).await;
    assert_eq!(
        described(&third.view().await),
        ["user:summary", "user:kept", "assistant:answer", "user:next", "system:other", "assistant:done"]
    );
    for setup in [setup, other, third] {
        setup.harness.close(&context()).await.unwrap();
    }
}

#[tokio::test]
async fn synthesizes_missing_tool_results_after_a_fork_and_drops_results_cut_from_their_call() {
    let setup = Setup::new().await;
    setup.message(user("go")).await;
    let call = setup.message(calling("calling", &["x", "y"], StopReason::Stop)).await;
    setup.message(tool_result("x")).await;
    let second = setup.message(tool_result("y")).await;
    let child = setup.root.fork(call.id, ownerless(), &context()).await.unwrap();
    let child_view = child.context(&context(), None).await.unwrap();
    assert_eq!(described(&child_view), ["user:go", "assistant:calling", "result:x:error", "result:y:error"]);
    let Message::ToolResult(missing) = &child_view.messages[2] else { panic!("expected a tool result") };
    assert_eq!(missing.tool_name, "tool-x");
    assert_eq!(missing.details.as_ref().unwrap()["reason"], "missing_result");

    // A head between a call and its results leaves stray results that are not sent.
    setup.append(EntryDraft { head: Some(DraftHead::Entry(second.id)), ..EntryDraft::new("reset") }).await;
    let parent_view = setup.view().await;
    assert!(described(&parent_view).is_empty());
    let kinds: Vec<&str> = parent_view.entries.iter().map(|entry| entry.kind.as_str()).collect();
    assert_eq!(kinds, ["reset", "message"]);
    setup.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn reads_the_context_as_of_an_earlier_entry_as_a_fork_at_that_entry_starts() {
    let setup = Setup::new().await;
    let first = setup.message(user("first")).await;
    let call = setup.message(calling("calling", &["x", "y"], StopReason::Stop)).await;
    let result = setup.message(tool_result("x")).await;
    setup.message(tool_result("y")).await;
    let replace =
        ContextEdit { target: first.id, action: ContextEditAction::Replace, messages: Some(vec![user("first v2")]) };
    let edit = setup.append(EntryDraft { edits: Some(vec![replace]), ..EntryDraft::new("edit") }).await;
    let reset = EntryDraft {
        head: Some(DraftHead::SelfEntry),
        model: Some(vec![user("fresh start")]),
        ..EntryDraft::new("reset")
    };
    let reset = setup.append(reset).await;
    let tail = setup.message(assistant("after reset", StopReason::Stop)).await;
    let root = &setup.root;
    for at in [&first, &call, &result, &edit, &reset, &tail] {
        let fork = root.fork(at.id, ownerless(), &context()).await.unwrap();
        let as_of = root.context(&context(), Some(at.id)).await.unwrap();
        assert_eq!(as_of, fork.context(&context(), None).await.unwrap());
    }
    // Stepped back before the edit and the reset: neither applies, and a cut call gets synthesized results.
    let stepped = root.context(&context(), Some(call.id)).await.unwrap();
    assert_eq!(described(&stepped), ["user:first", "assistant:calling", "result:x:error", "result:y:error"]);
    let whole = setup.view().await;
    assert_eq!(root.context(&context(), Some(tail.id)).await.unwrap(), whole);

    let other = root.fork(first.id, ownerless(), &context()).await.unwrap();
    let error = other.context(&context(), Some(tail.id)).await.unwrap_err().to_string();
    assert!(error.contains("is not visible"), "{error}");
    setup.harness.close(&context()).await.unwrap();
}
