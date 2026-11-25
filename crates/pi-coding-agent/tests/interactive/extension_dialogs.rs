//! Concurrent extension dialogs and inline `custom()` components stack in the editor slot:
//! closing the visible one shows the one below it again and no dialog is dropped unanswered.

use pi_tui::program_status::{BlockedKind, ProgramState};

use crate::interactive::harness::{Options, Ui, local, start};

const EXTENSION: &str = r#"import { Text } from "@earendil-works/pi-tui";
export default function (pi) {
	pi.registerCommand("stack", { handler: async (_args, ctx) => {
		const first = ctx.ui.confirm("First", "first dialog");
		const second = ctx.ui.confirm("Second", "second dialog");
		ctx.ui.notify(`results ${await first} ${await second}`);
	} });
	pi.registerCommand("abort-hidden", { handler: async (_args, ctx) => {
		const controller = new AbortController();
		const first = ctx.ui.confirm("First", "first dialog", { signal: controller.signal });
		const second = ctx.ui.confirm("Second", "second dialog");
		await new Promise((resolve) => setTimeout(resolve, 300));
		controller.abort();
		ctx.ui.notify(`results ${await first} ${await second}`);
	} });
	pi.registerCommand("custom-stack", { handler: async (_args, ctx) => {
		let second;
		const first = ctx.ui.custom((_tui, _theme, _keys, done) => ({
			render: () => ["CUSTOM-ONE"], invalidate() {},
			handleInput: (data) => {
				if (data === "n") {
					second = ctx.ui.custom((_t, _th, _k, finish) => ({
						render: () => ["CUSTOM-TWO"], invalidate() {}, handleInput: (key) => key === "d" && finish("two"),
					}));
				} else if (data === "d") done("one");
			},
		}));
		ctx.ui.notify(`custom ${await first} ${await second}`);
	} });
	pi.registerCommand("confirm-in-custom", { handler: async (_args, ctx) => {
		let discarded;
		const result = await ctx.ui.custom((_tui, _theme, _keys, done) => ({
			render: () => ["CUSTOM-FORM"], invalidate() {},
			handleInput: (data) => {
				if (data === "x") ctx.ui.confirm("Discard?", "discard changes").then((answer) => { discarded = answer; });
				else if (data === "d") done("kept");
			},
		}));
		ctx.ui.notify(`form ${result} discarded ${discarded}`);
	} });
	pi.registerCommand("blocked-custom", { handler: async (_args, ctx) => {
		let loader;
		const first = ctx.ui.custom((_tui, _theme, _keys, done) => ({
			render: () => ["APPROVAL"], invalidate() {},
			handleInput: (data) => {
				if (data === "n") {
					loader = ctx.ui.custom((_t, _th, _k, finish) => ({
						render: () => ["LOADING"], invalidate() {}, handleInput: (key) => key === "d" && finish("loaded"),
					}));
				} else if (data === "d") done("yes");
			},
		}), { blocked: { kind: "permission", message: "Approve?" } });
		ctx.ui.notify(`blocked ${await first} ${await loader}`);
	} });
	pi.registerCommand("blocked-overlays", { handler: async (_args, ctx) => {
		const dones = [];
		const handles = [];
		const open = (text, onKey, blocked) =>
			ctx.ui.custom((_tui, _theme, _keys, done) => {
				dones.push(done);
				return { render: () => [text], invalidate() {}, handleInput: onKey };
			}, { overlay: true, onHandle: (handle) => handles.push(handle), blocked });
		let second;
		let loader;
		const first = open("FIRST", (key) => {
			if (key === "n") {
				second = open("SECOND", (key) => {
					if (key === "n") loader = open("LOADER", (key) => key === "d" && dones[2]("loaded"));
					else if (key === "h") handles[0].setHidden(true);
					else if (key === "d") dones[1]("two");
				}, { kind: "question", message: "Second?" });
			}
		}, { kind: "question", message: "First?" });
		const off = ctx.ui.onTerminalInput((data) => {
			if (data !== "z") return undefined;
			off();
			dones[0]("one");
			return { consume: true };
		});
		ctx.ui.notify(`overlays ${await first} ${await second} ${await loader}`);
	} });
	pi.registerCommand("blocked-overlay-reload", { handler: async (_args, ctx) => {
		void ctx.ui.custom(() => ({ render: () => ["QUESTION"], invalidate() {} }), {
			overlay: true,
			overlayOptions: { nonCapturing: true },
			blocked: { kind: "question", message: "Question?" },
		});
	} });
	pi.registerCommand("swap-editor", { handler: async (_args, ctx) => {
		const answer = ctx.ui.confirm("Swap", "swap dialog");
		await new Promise((resolve) => setTimeout(resolve, 100));
		ctx.ui.setEditorComponent(() => ({
			render: () => ["CUSTOM-EDITOR"], invalidate() {}, getText: () => "", setText() {}, handleInput() {},
		}));
		ctx.ui.notify(`swap ${await answer}`);
	} });
}
"#;

fn showing(ui: &Ui, text: &str) -> bool {
    ui.screen().contains(text)
}

async fn start_ui() -> Ui {
    start(Options { files: vec![(".pi/extensions/dialogs.ts", EXTENSION)], ..Options::default() }).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_earlier_confirm_returns_after_the_later_one_is_answered() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/stack").await;
        ui.wait_until("second dialog", |ui| showing(ui, "second dialog")).await;
        assert!(!showing(&ui, "first dialog"), "{}", ui.screen());
        // Escape dismisses the visible dialog, which answers false.
        ui.send("\x1b").await;
        ui.wait_until("first dialog again", |ui| showing(ui, "first dialog")).await;
        assert!(!showing(&ui, "second dialog"), "{}", ui.screen());
        ui.send("\r").await;
        ui.wait_for("results true false").await;
        ui.wait_until("editor restored", |ui| !showing(ui, "first dialog")).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hidden_dialog_is_removed_when_its_signal_aborts() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/abort-hidden").await;
        ui.wait_until("second dialog", |ui| showing(ui, "second dialog")).await;
        ui.settle(500).await;
        assert!(showing(&ui, "second dialog"), "{}", ui.screen());
        ui.send("\r").await;
        ui.wait_for("results false true").await;
        ui.wait_until("editor restored", |ui| !showing(ui, "second dialog") && !showing(ui, "first dialog")).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_inline_custom_components_stack() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/custom-stack").await;
        ui.wait_until("first component", |ui| showing(ui, "CUSTOM-ONE")).await;
        ui.send("n").await;
        ui.wait_until("second component", |ui| showing(ui, "CUSTOM-TWO")).await;
        assert!(!showing(&ui, "CUSTOM-ONE"), "{}", ui.screen());
        ui.send("d").await;
        ui.wait_until("first component again", |ui| showing(ui, "CUSTOM-ONE")).await;
        ui.send("d").await;
        ui.wait_for("custom one two").await;
        ui.wait_until("editor restored", |ui| !showing(ui, "CUSTOM-ONE")).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_confirm_opened_from_a_custom_component_restores_it() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/confirm-in-custom").await;
        ui.wait_until("component", |ui| showing(ui, "CUSTOM-FORM")).await;
        ui.send("x").await;
        ui.wait_until("confirm", |ui| showing(ui, "discard changes")).await;
        ui.send("\x1b").await;
        ui.wait_until("component again", |ui| showing(ui, "CUSTOM-FORM")).await;
        assert!(!showing(&ui, "discard changes"), "{}", ui.screen());
        ui.send("d").await;
        ui.wait_for("form kept discarded false").await;
    })
    .await;
}

// Upstream #11431: replacing the editor while a dialog was open hid the dialog unanswered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_dialog_stays_visible_when_the_editor_component_is_replaced() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/swap-editor").await;
        ui.wait_until("dialog", |ui| showing(ui, "swap dialog")).await;
        ui.settle(500).await;
        assert!(showing(&ui, "swap dialog") && !showing(&ui, "CUSTOM-EDITOR"), "{}", ui.screen());
        ui.send("\r").await;
        ui.wait_for("swap true").await;
        ui.wait_until("custom editor", |ui| showing(ui, "CUSTOM-EDITOR")).await;
    })
    .await;
}

/// Whether the latest program status report is `blocked` with `kind` and `message`.
fn reports_blocked(ui: &Ui, expected: Option<(BlockedKind, &str)>) -> bool {
    let Some(last) = ui.program_statuses().pop() else { return expected.is_none() };
    match expected {
        Some((kind, message)) => {
            last.state == ProgramState::Blocked && last.kind == Some(kind) && last.message.as_deref() == Some(message)
        }
        None => last.state != ProgramState::Blocked,
    }
}

// Upstream #11428: inline custom dialogs could not report blocked program status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inline_custom_component_can_report_blocked_status() {
    local(async {
        let ui = start_ui().await;
        let approval = Some((BlockedKind::Permission, "Approve?"));
        ui.submit("/blocked-custom").await;
        ui.wait_until("blocked approval", |ui| showing(ui, "APPROVAL") && reports_blocked(ui, approval)).await;
        ui.send("n").await;
        ui.wait_until("unblocked loader", |ui| showing(ui, "LOADING") && reports_blocked(ui, None)).await;
        ui.send("d").await;
        ui.wait_until("blocked again", |ui| showing(ui, "APPROVAL") && reports_blocked(ui, approval)).await;
        ui.send("d").await;
        ui.wait_for("blocked yes loaded").await;
        ui.wait_until("cleared", |ui| reports_blocked(ui, None)).await;
    })
    .await;
}

// Upstream #11816: overlays ignored the blocked option.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_overlays_report_blocked_status_until_done_also_while_hidden() {
    local(async {
        let ui = start_ui().await;
        let first = Some((BlockedKind::Question, "First?"));
        let second = Some((BlockedKind::Question, "Second?"));
        ui.submit("/blocked-overlays").await;
        ui.wait_until("first overlay", |ui| showing(ui, "FIRST") && reports_blocked(ui, first)).await;
        ui.send("n").await;
        ui.wait_until("second overlay", |ui| showing(ui, "SECOND") && reports_blocked(ui, second)).await;
        ui.send("n").await;
        ui.wait_until("loader", |ui| showing(ui, "LOADER")).await;
        assert!(reports_blocked(&ui, second), "a loader overlay does not report blocked");
        ui.send("d").await;
        ui.wait_until("loader closed", |ui| !showing(ui, "LOADER")).await;
        assert!(reports_blocked(&ui, second));
        ui.send("h").await;
        ui.wait_until("first hidden", |ui| !showing(ui, "FIRST")).await;
        ui.send("d").await;
        ui.wait_until("second closed", |ui| !showing(ui, "SECOND")).await;
        ui.settle(200).await;
        assert!(reports_blocked(&ui, first), "a hidden overlay stays blocked until done");
        ui.send("z").await;
        ui.wait_for("overlays one two loaded").await;
        ui.wait_until("cleared", |ui| reports_blocked(ui, None)).await;
    })
    .await;
}

// Upstream #11816: /reload drops overlays without calling done().
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_clears_the_blocked_status_of_open_overlays() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/blocked-overlay-reload").await;
        let question = Some((BlockedKind::Question, "Question?"));
        ui.wait_until("blocked overlay", |ui| showing(ui, "QUESTION") && reports_blocked(ui, question)).await;
        ui.submit("/reload").await;
        ui.wait_until("cleared", |ui| reports_blocked(ui, None)).await;
    })
    .await;
}
