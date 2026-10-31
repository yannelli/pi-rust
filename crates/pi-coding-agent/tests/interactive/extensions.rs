//! Extensions in the interactive mode: footer statuses, widgets around the editor, commands in
//! the slash menu, dialogs in the editor slot, message and entry renderers, and shortcuts.

use pi_ai::providers::faux::FauxResponse;

use crate::harness::{Options, local, start};

const EXTENSION: &str = r#"import { Text } from "@earendil-works/pi-tui";
export default function (pi) {
	pi.on("session_start", (_event, ctx) => {
		ctx.ui.setStatus("ext", "EXT-STATUS");
		ctx.ui.setWidget("above", ["WIDGET-ABOVE"]);
		ctx.ui.setWidget("below", (_tui, _theme) => new Text("WIDGET-BELOW", 1, 0), { placement: "belowEditor" });
	});
	pi.registerMessageRenderer("card", (message) => new Text(`CARD<${message.content}>`, 1, 0));
	pi.registerEntryRenderer("note", (entry, options) => new Text(`NOTE<${entry.data.color} ${options.expanded}>`, 1, 0));
	pi.registerCommand("pick-color", {
		description: "Pick a color",
		handler: async (_args, ctx) => {
			const color = await ctx.ui.select("Pick a color", ["Red", "Blue"]);
			ctx.ui.notify(`picked ${color}`);
			pi.sendMessage({ customType: "card", content: color, display: true });
			pi.appendEntry("note", { color });
		},
	});
	pi.registerShortcut("ctrl+shift+k", { description: "Clear widgets", handler: async (ctx) => {
		ctx.ui.setWidget("above", undefined);
		ctx.ui.notify("shortcut ran");
	} });
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_ui_shows_in_the_interactive_mode() {
    local(async {
        let ui = start(Options {
            files: vec![(".pi/extensions/ui.ts", EXTENSION)],
            responses: vec![FauxResponse::text("unused")],
            ..Options::default()
        })
        .await;
        ui.wait_for("WIDGET-ABOVE").await;
        ui.wait_for("WIDGET-BELOW").await;
        ui.wait_for("EXT-STATUS").await;
        let screen = ui.screen();
        let rows: Vec<&str> = screen.lines().collect();
        let row = |text: &str| rows.iter().position(|line| line.contains(text)).unwrap();
        assert!(row("WIDGET-ABOVE") < row("WIDGET-BELOW"), "{screen}");

        ui.send("/pick-c").await;
        ui.wait_for("Pick a color").await;
        ui.send("\x03").await;
        ui.submit("/pick-color").await;
        ui.wait_until("selector", |ui| ui.screen().contains("Red") && ui.screen().contains("Blue")).await;
        ui.send("\x1b[B").await;
        ui.send("\r").await;
        ui.wait_for("picked Blue").await;
        ui.wait_for("CARD<Blue>").await;
        ui.wait_for("NOTE<Blue false>").await;

        ui.send("\x1b[107;6u").await;
        ui.wait_for("shortcut ran").await;
        ui.wait_until("widget removed", |ui| !ui.screen().contains("WIDGET-ABOVE")).await;
        assert!(ui.provider.requests().is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_native_panel_repaints_after_a_height_only_resize() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native/fixture.mjs");
    let package = serde_json::json!({"name": "native-fixture", "pi": {"native": [{
        "command": env!("CARGO_BIN_EXE_pi"),
        "args": [fixture.display().to_string()],
        "env": {"PI_JS_FORK": "1"},
    }]}});
    let package: &'static str = Box::leak(package.to_string().into_boxed_str());
    local(async {
        let ui =
            start(Options { files: vec![(".pi/extensions/native/package.json", package)], ..Options::default() }).await;
        ui.submit("/native-panel").await;
        ui.wait_until("30 rows", |ui| ui.screen().contains("height 30")).await;
        ui.terminal.borrow_mut().resize(100, 12);
        ui.wait_until("12 rows", |ui| ui.screen().contains("height 12")).await;
        ui.terminal.borrow_mut().resize(60, 12);
        ui.wait_until("60 columns", |ui| ui.screen().contains("width 60 height 12")).await;
    })
    .await;
}
