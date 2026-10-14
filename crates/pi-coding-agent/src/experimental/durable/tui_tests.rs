//! Rendering checks of the durable TUI on a virtual terminal. Upstream has no tests for `durable/tui.ts`.

use std::path::PathBuf;
use std::rc::Rc;

use pi_durable::ConversationId;
use pi_tui::env::MapEnv;
use pi_tui::testing::VirtualTerminal;
use pi_tui::{Tui, TuiAltScreenOptions, TuiOptions, ui_loop};
use serde_json::{Value, json};

use super::runtime::{ConversationSummary, DurableView, Notice, NoticeLevel, SessionSummary};
use super::tui::DurableTui;
use super::tui_parts::{context_tokens, describe_task, total_usage};
use crate::interactive_components::test_support::setup;

fn usage(total: u64) -> Value {
    json!({ "input": total, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": total,
        "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.25 } })
}

fn entries() -> Value {
    let answer = json!({
        "role": "assistant", "api": "faux", "provider": "faux", "model": "m", "usage": usage(1200),
        "stopReason": "toolUse", "timestamp": 2,
        "content": [{ "type": "text", "text": "hi there" },
            { "type": "toolCall", "id": "call-1", "name": "subagent", "arguments": { "task": "look" } }],
    });
    json!([
        { "kind": "pi.user", "id": 1, "conversationId": 1, "model": [{ "role": "user", "content": "hello", "timestamp": 1 }] },
        { "kind": "pi.assistant", "id": 2, "conversationId": 1, "model": [answer] },
    ])
}

fn view() -> DurableView {
    let live = json!({ "run": { "id": 1 }, "tools": [{ "status": "running", "name": "subagent", "callId": "call-1",
        "details": { "conversationId": 2 } }] });
    let task = json!({ "id": 7, "kind": "pi.tool", "state": { "status": "running", "phase": "execute" },
        "conversationId": 1, "conversations": [2], "background": false, "abortRequested": false });
    DurableView {
        session: SessionSummary { id: "s".into(), directory: PathBuf::from("/tmp/s"), cwd: "/work".into() },
        conversation: json!({ "conversation": { "id": 1 }, "entries": entries(), "docs": {
            "pi.live": live, "pi.inbox": { "items": [{ "mode": "followUp", "content": "later" }] },
            "pi.usage": { "models": { "faux/m": usage(1200) }, "tools": {} } } }),
        conversations: vec![ConversationSummary { id: ConversationId(1), label: "main".into(), title: None }],
        models: Vec::new(),
        notices: vec![Notice { id: 1, level: NoticeLevel::Warning, message: "No model configured".into() }],
        tasks: Some(json!({ "tasks": { "7": task } })),
    }
}

#[test]
fn renders_the_transcript_running_subagent_dock_and_footer() {
    let _keys = setup();
    let terminal = VirtualTerminal::shared(100, 30);
    let options = TuiOptions { env: Some(Rc::new(MapEnv::default())), ..TuiOptions::default() };
    let tui = Tui::new_alt_screen(terminal.clone(), options, TuiAltScreenOptions::default());
    let view = DurableTui::new(tui.clone(), PathBuf::from("/work"));
    tui.start();
    view.apply(&self::view());
    ui_loop::run_pending().unwrap();
    tui.render_now(true).unwrap();
    let screen = terminal.borrow().viewport().join("\n");
    for expected in [
        "hello",
        "hi there",
        "Subagent 2 is working. /agents switches to it.",
        "Tasks (1 live, /tasks to hide)",
        "pi.tool #7: running execute owns conversation 2",
        "[followUp] later",
        "No model configured",
        "↑1.2k $0.250  /work",
        "main · no model · thinking:off",
    ] {
        assert!(screen.contains(expected), "missing {expected:?} in\n{screen}");
    }
    view.stop();
}

#[test]
fn describes_tasks_and_measures_usage_and_context() {
    let waiting = json!({ "id": 3, "kind": "k", "state": { "status": "waiting", "on": [4, 5] },
        "conversations": [], "background": true, "abortRequested": true });
    assert_eq!(describe_task(&waiting), "k #3: waiting on 4, 5 [background, aborting]");
    let total = total_usage(&json!({ "models": { "a": usage(10) }, "tools": { "b": usage(5) } }));
    assert_eq!((total.input, total.cost), (15, 0.5));
    let records: Vec<pi_durable::EntryRecord> = serde_json::from_value(entries()).unwrap();
    assert_eq!(context_tokens(&records), Some(1200));
    assert_eq!(context_tokens(&records[..1]), None);
}
