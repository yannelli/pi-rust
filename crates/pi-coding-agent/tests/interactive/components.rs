//! Upstream interactive components from `@earendil-works/pi-coding-agent` in `ctx.ui.custom`: they
//! draw in the Rust terminal, take keys, and resolve the custom call.

use crate::harness::{Options, Ui, local, start};

const EXTENSION: &str = r#"import { AssistantMessageComponent, ExtensionInputComponent, ExtensionSelectorComponent } from "@earendil-works/pi-coding-agent";
export default function (pi) {
	pi.registerCommand("pick", { handler: async (_args, ctx) => {
		const picked = await ctx.ui.custom((tui, _theme, _keys, done) =>
			new ExtensionSelectorComponent("Pick a fruit", ["apple", "banana"], done, () => done(null), { tui }));
		ctx.ui.notify(`picked ${picked}`);
	} });
	pi.registerCommand("ask", { handler: async (_args, ctx) => {
		const name = await ctx.ui.custom((tui, _theme, _keys, done) =>
			new ExtensionInputComponent("Your name?", undefined, done, () => done("cancelled"), { tui, initialValue: "Ada" }));
		ctx.ui.notify(`named ${name}`);
	} });
	pi.registerCommand("show", { handler: async (_args, ctx) => {
		const message = { role: "assistant", content: [{ type: "text", text: "Rendered by **upstream**" }], api: "faux",
			provider: "faux", model: "m", stopReason: "stop", timestamp: 0,
			usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } } };
		const shown = await ctx.ui.custom((_tui, _theme, _keys, done) => {
			const component = new AssistantMessageComponent(message);
			component.handleInput = () => done("closed");
			return component;
		});
		ctx.ui.notify(`show ${shown}`);
	} });
}
"#;

async fn start_ui() -> Ui {
    start(Options { files: vec![(".pi/extensions/components.ts", EXTENSION)], ..Options::default() }).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_selector_component_picks_with_arrow_keys() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/pick").await;
        ui.wait_until("selector", |ui| ui.screen().contains("Pick a fruit") && ui.screen().contains("apple")).await;
        assert!(ui.screen().contains("banana"), "{}", ui.screen());
        ui.send("\x1b[B").await;
        ui.send("\r").await;
        ui.wait_for("picked banana").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_input_component_submits_its_prefill_and_escape_cancels() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/ask").await;
        ui.wait_until("input", |ui| ui.screen().contains("Your name?") && ui.screen().contains("Ada")).await;
        ui.send("\r").await;
        ui.wait_for("named Ada").await;
        ui.submit("/ask").await;
        ui.wait_until("input again", |ui| ui.screen().contains("Your name?")).await;
        ui.send("\x1b").await;
        ui.wait_for("named cancelled").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assistant_message_component_draws_markdown_in_the_terminal() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/show").await;
        ui.wait_until("message", |ui| ui.screen().contains("Rendered by upstream")).await;
        assert!(!ui.screen().contains("**upstream**"), "{}", ui.screen());
        ui.send("x").await;
        ui.wait_for("show closed").await;
    })
    .await;
}
