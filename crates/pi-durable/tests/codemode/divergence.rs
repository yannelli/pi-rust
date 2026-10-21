//! Reruns that take another path than the journal and the earlier execution's calls say: each ends the call with an
//! error result and admits no call.

use super::restarts::{Restarted, acknowledged, edit_journal, journal, no_change, restart_with, set_store};
use super::*;

fn assert_ends(restarted: &Restarted, text: &str) {
    let (result, is_error) = &restarted.result;
    assert!(*is_error, "{result}");
    assert!(result.contains(text), "{result}");
}

#[tokio::test]
async fn ends_the_call_when_a_resumed_script_made_a_different_number_of_calls_than_before() {
    let chat = codemode_chat(vec![]);
    let change = async |harness: Harness, task: TaskId| {
        edit_journal(&harness, task, |journal| journal.entries[0].issued_after = Some(7)).await;
    };
    let code = r#"await tools.echo({ text: "a" }); await tools.hold({});"#;
    let restarted = restart_with(&chat, code, change, no_change).await;
    assert_ends(&restarted, "after result 1 it had made 2 calls, where it had made 7");
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_the_call_at_once_when_a_resumed_script_waits_without_making_a_call_it_made_before() {
    let chat = codemode_chat(vec![]);
    let change = async |harness: Harness, task: TaskId| {
        edit_journal(&harness, task, |journal| journal.entries[0].id = 99).await;
    };
    let code = r#"await tools.echo({ text: "a" }); await tools.hold({});"#;
    let restarted = restart_with(&chat, code, change, no_change).await;
    assert_ends(&restarted, "it waits without making call 99");
    // hold, admitted before the restart, resumed on its own; no call was admitted by the replay.
    assert_eq!(nested_count(&restarted.harness).await, 2);
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_the_call_when_a_resumed_script_calls_a_global_with_other_arguments() {
    let chat = codemode_chat(vec![CodemodeGlobal::new("next", |_, _, _| async { Ok(Some(json!(1))) })]);
    let change = async |harness: Harness, task: TaskId| {
        edit_journal(&harness, task, |journal| journal.entries[0].global.as_mut().unwrap().args = Some(json!("other")))
            .await;
    };
    let restarted = restart_with(&chat, "await next(); await tools.hold({});", change, no_change).await;
    assert_ends(&restarted, "call 1 is next");
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn admits_no_call_of_a_diverging_replay_even_one_the_script_never_made_before() {
    let chat = codemode_chat(vec![]);
    // With k set, the replay makes three extra calls after result 1, which the live run did not make; the count in
    // result 1's acknowledgment shows the divergence only after they were issued.
    let code = r#"await tools.echo({ text: "a" });
        const extra = load("k") ?? 0;
        for (let i = 0; i < extra; i++) tools.echo({ text: "extra" + i });
        await tools.hold({});"#;
    let change = async |harness: Harness, task: TaskId| set_store(&harness, task, json!({ "k": 3 })).await;
    let before = async |harness: Harness, task: TaskId| acknowledged(&harness, task, 2).await;
    let restarted = restart_with(&chat, code, change, before).await;
    assert_ends(&restarted, "after result 1 it had made 5 calls, where it had made 2");
    assert_eq!(echoes(&chat), BTreeMap::from([("a".into(), 1)]));
    assert_eq!(nested_count(&restarted.harness).await, 2);
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_the_call_when_a_resumed_script_ends_where_it_went_on_before() {
    let chat = codemode_chat(vec![]);
    let code = r#"await tools.echo({ text: "a" }); if (load("k")) return "early"; await tools.hold({});"#;
    let change = async |harness: Harness, task: TaskId| set_store(&harness, task, json!({ "k": true })).await;
    // The live run acknowledged result 1 before the restart: it went on after it.
    let before = async |harness: Harness, task: TaskId| acknowledged(&harness, task, 2).await;
    let restarted = restart_with(&chat, code, change, before).await;
    assert_ends(&restarted, "it ended after result 1, where it went on before");
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_the_call_when_a_resumed_script_ends_before_calls_the_live_run_made_even_without_a_recorded_count() {
    let chat = codemode_chat(vec![]);
    let code = r#"await tools.echo({ text: "a" }); if (load("k")) return "early"; await tools.echo({ text: "b" }); await tools.hold({});"#;
    let change = async |harness: Harness, task: TaskId| {
        set_store(&harness, task, json!({ "k": true })).await;
        // As if the live run crashed before recording result 1's count.
        edit_journal(&harness, task, |journal| journal.entries[0].issued_after = None).await;
    };
    let restarted = restart_with(&chat, code, change, no_change).await;
    assert_ends(&restarted, "it ended after result 1, where it went on before");
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_the_call_when_a_resumed_script_makes_a_call_an_earlier_execution_admitted_with_other_arguments() {
    let chat = codemode_chat(vec![]);
    let change = async |harness: Harness, task: TaskId| {
        set_store(&harness, task, json!({ "k": "other" })).await;
        // As if the restart came before the first journal commit: the rerun runs live from the start.
        edit_journal(&harness, task, |journal| journal.entries.clear()).await;
    };
    let code = r#"await tools.echo({ text: load("k") ?? "a" }); await tools.hold({});"#;
    let restarted = restart_with(&chat, code, change, no_change).await;
    assert_ends(&restarted, "different path");
    assert_ends(&restarted, "call 1 is tools.echo");
    assert_eq!(echoes(&chat), BTreeMap::from([("a".into(), 1)]));
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn checks_a_rerun_s_calls_against_an_earlier_execution_s_before_admitting_any_later_call() {
    let chat = codemode_chat(vec![]);
    // With k set, call 1 has other arguments and call 2 is another tool than before; call 3 would be new.
    let code = r#"const k = load("k");
        tools.echo({ text: k ?? "a" });
        if (k) tools.echo({ text: "new" });
        await tools.hold({});"#;
    let change = async |harness: Harness, task: TaskId| {
        set_store(&harness, task, json!({ "k": "x" })).await;
        // As if the restart came before the first journal commit.
        edit_journal(&harness, task, |journal| journal.entries.clear()).await;
    };
    let restarted = restart_with(&chat, code, change, no_change).await;
    // The first run already made more calls than before.
    assert_ends(&restarted, "its first run made 3 calls, where it had made 2");
    assert_eq!(nested_count(&restarted.harness).await, 2);
    assert_eq!(echoes(&chat), BTreeMap::from([("a".into(), 1)]));
    restarted.harness.close(&ctx()).await.unwrap();
}

fn pick() -> CodemodeGlobal {
    CodemodeGlobal::new("pick", |_, _, _| async { Ok(Some(json!(1))) })
}

#[tokio::test]
async fn ends_the_call_when_a_rerun_makes_a_global_call_where_an_earlier_execution_made_a_tool_call() {
    let chat = codemode_chat(vec![pick()]);
    let code = r#"if (load("k")) { await pick(); await tools.echo({ text: "new" }); } else { await tools.hold({}); }"#;
    let change = async |harness: Harness, task: TaskId| set_store(&harness, task, json!({ "k": true })).await;
    let restarted = restart_with(&chat, code, change, no_change).await;
    assert_ends(&restarted, "call 1 is pick(), but was tools.hold({})");
    assert!(echoes(&chat).is_empty());
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_the_call_when_a_rerun_s_first_run_makes_fewer_calls_than_an_earlier_execution_admitted() {
    // Live, the first run admits hold 1 and hold 2. With k set, it makes only hold 1, and after its result a global
    // and a new tool call: the global's ID is hold 2's key, which no check after the release would see.
    let chat = codemode_chat(vec![pick()]);
    let code = r#"const k = load("k");
        const first = tools.hold({ id: 1 });
        if (!k) tools.hold({ id: 2 });
        await first;
        await pick();
        await tools.echo({ text: "new" });"#;
    let change = async |harness: Harness, task: TaskId| set_store(&harness, task, json!({ "k": true })).await;
    let before = async |harness: Harness, task: TaskId| {
        eventually(|| async { journal(&harness, task).await.and_then(|journal| journal.first_issued) == Some(2) })
            .await;
    };
    let restarted = restart_with(&chat, code, change, before).await;
    assert_ends(&restarted, "its first run made 1 calls, where it had made 2");
    assert!(echoes(&chat).is_empty());
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_the_call_when_a_rerun_without_journal_entries_ends_in_its_first_run_where_it_went_on_before() {
    let chat = codemode_chat(vec![]);
    let code = r#"if (load("k")) { store("x", 1); return "early"; } await tools.hold({});"#;
    let change = async |harness: Harness, task: TaskId| set_store(&harness, task, json!({ "k": true })).await;
    let before = async |harness: Harness, task: TaskId| {
        eventually(|| async { journal(&harness, task).await.and_then(|journal| journal.first_issued) == Some(1) })
            .await;
    };
    let restarted = restart_with(&chat, code, change, before).await;
    assert_ends(&restarted, "it ended after result 0, where it went on before");
    restarted.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn checks_every_call_a_rerun_holds_before_launching_any_of_them() {
    // Live, call 1 is a global that never finishes and call 2 is hold; with k set, call 1 is a new tool call and
    // call 2 holds with other arguments. Checking call 1 alone would launch it before call 2's conflict shows.
    let forever = CodemodeGlobal::new("forever", |_, _, context: Context| async move {
        aborted(context.abort_signal().unwrap()).await;
        Ok(Some(Value::Null))
    });
    let chat = codemode_chat(vec![forever]);
    let code = r#"const k = load("k");
        if (k) tools.echo({ text: "new" }); else forever();
        await tools.hold({ id: k ? 2 : 1 });"#;
    let change = async |harness: Harness, task: TaskId| set_store(&harness, task, json!({ "k": true })).await;
    let restarted = restart_with(&chat, code, change, no_change).await;
    assert_ends(&restarted, r#"call 2 is tools.hold({"id":2}), but was tools.hold({"id":1})"#);
    assert!(echoes(&chat).is_empty());
    assert_eq!(nested_count(&restarted.harness).await, 1);
    restarted.harness.close(&ctx()).await.unwrap();
}
