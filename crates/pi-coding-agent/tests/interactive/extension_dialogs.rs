//! Concurrent extension dialogs and inline `custom()` components stack in the editor slot:
//! closing the visible one shows the one below it again and no dialog is dropped unanswered.

use crate::harness::{Options, Ui, local, start};

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
