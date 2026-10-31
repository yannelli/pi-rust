//! Quitting stays bounded when extension cleanup or a running tool never finishes (#133).

use pi_ai::providers::faux::FauxResponse;
use serde_json::json;

use crate::harness::{Options, Ui, local, start};

const HANGING_SHUTDOWN: &str = r#"export default function (pi) {
	pi.on("session_start", (_event, ctx) => ctx.ui.notify("HANG-READY"));
	pi.on("session_shutdown", () => new Promise((resolve) => setTimeout(resolve, 600000)));
}
"#;

const SLOW_CLEANUP: &str = r#"import { writeFileSync } from "node:fs";
export default function (pi) {
	pi.on("session_shutdown", async (event, ctx) => {
		await new Promise((resolve) => setTimeout(resolve, 300));
		writeFileSync(`${ctx.cwd}/cleanup-done.txt`, event.reason);
	});
}
"#;

const HANGING_TOOL: &str = r#"import { Type } from "typebox";
export default function (pi) {
	pi.registerTool({
		name: "stall", label: "Stall", description: "Never returns", parameters: Type.Object({}),
		async execute() {
			pi.appendEntry("stall-started", {});
			return new Promise((resolve) => setTimeout(resolve, 600000));
		},
	});
}
"#;

fn saved_session(ui: &Ui) -> String {
    let file = ui.session().session_file().expect("persisted session");
    std::fs::read_to_string(file).unwrap()
}

/// The session file still parses line by line after quitting.
fn assert_session_readable(ui: &Ui, text: &str) {
    let saved = saved_session(ui);
    for line in saved.lines() {
        serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|error| panic!("{error}: {line}"));
    }
    assert!(saved.contains(text), "{saved}");
}

async fn quit_with_hanging_shutdown(keys: &str) {
    let responses = vec![FauxResponse::text("kept answer")];
    let ui =
        start(Options { responses, files: vec![(".pi/extensions/hang.ts", HANGING_SHUTDOWN)], ..Options::default() })
            .await;
    ui.wait_for("HANG-READY").await;
    ui.submit("hello").await;
    ui.wait_for("kept answer").await;
    match keys {
        "/quit" => ui.submit(keys).await,
        "double-ctrl-c" => {
            ui.send("\x03").await;
            ui.send("\x03").await;
        }
        _ => ui.send(keys).await,
    }
    ui.wait_until("exit", |ui| ui.mode.is_exited()).await;
    assert_session_readable(&ui, "kept answer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slash_quit_finishes_when_session_shutdown_never_returns() {
    local(quit_with_hanging_shutdown("/quit")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_d_finishes_when_session_shutdown_never_returns() {
    local(quit_with_hanging_shutdown("\x04")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn double_ctrl_c_finishes_when_session_shutdown_never_returns() {
    local(quit_with_hanging_shutdown("double-ctrl-c")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quit_waits_for_a_session_shutdown_handler_that_finishes() {
    local(async {
        let ui = start(Options { files: vec![(".pi/extensions/slow.ts", SLOW_CLEANUP)], ..Options::default() }).await;
        ui.submit("/quit").await;
        ui.wait_until("exit", |ui| ui.mode.is_exited()).await;
        assert_eq!(std::fs::read_to_string(ui.cwd().join("cleanup-done.txt")).unwrap(), "quit");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quit_finishes_while_an_extension_tool_ignores_abort() {
    local(async {
        let responses = vec![FauxResponse::tool_call("call-1", "stall", json!({}))];
        let ui =
            start(Options { responses, files: vec![(".pi/extensions/stall.ts", HANGING_TOOL)], ..Options::default() })
                .await;
        ui.submit("run it").await;
        ui.wait_until("tool started", |ui| saved_session(ui).contains("stall-started")).await;
        ui.send("\x04").await;
        ui.wait_until("exit", |ui| ui.mode.is_exited()).await;
        assert_session_readable(&ui, "stall-started");
    })
    .await;
}
