//! Interactive behavior ported from upstream `interactive-mode.ts`: extension autocomplete
//! providers, `custom()` rejection and overlay handles, self-framed tool renderers, `/import` of
//! a session whose folder is gone, and catalog refreshes from `/model`.

use pi_ai::providers::faux::FauxResponse;
use serde_json::json;

use crate::harness::{Options, local, start};

const EXTENSION: &str = r#"import { Text } from "@earendil-works/pi-tui";
import { Type } from "typebox";
export default function (pi) {
	pi.on("session_start", (_event, ctx) => {
		ctx.ui.addAutocompleteProvider((current) => ({
			triggerCharacters: ["%"],
			async getSuggestions(lines, line, col, options) {
				if (lines[line].slice(0, col).endsWith("%co")) {
					return { items: [{ value: "%color", label: "%color", description: "EXT-COMPLETION" }], prefix: "%co" };
				}
				return current.getSuggestions(lines, line, col, options);
			},
			applyCompletion: (...args) => current.applyCompletion(...args),
		}));
	});
	pi.registerCommand("broken", { handler: async (_args, ctx) => {
		try {
			await ctx.ui.custom(() => { throw new Error("factory boom"); });
			ctx.ui.notify("custom resolved");
		} catch (error) {
			ctx.ui.notify(`custom rejected: ${error.message}`);
		}
	} });
	pi.registerCommand("overlay", { handler: async (_args, ctx) => {
		let finish;
		const result = await ctx.ui.custom((_tui, _theme, _keys, done) => {
			finish = done;
			return new Text("OVERLAY-BODY", 0, 0);
		}, { overlay: true, onHandle: (handle) => {
			const before = handle.isHidden();
			handle.setHidden(true);
			finish(`closed ${before} ${handle.isHidden()}`);
		} });
		ctx.ui.notify(`overlay ${result}`);
	} });
	pi.registerTool({
		name: "selfie", label: "Selfie", description: "Draws itself", parameters: Type.Object({}),
		renderShell: "self",
		renderCall: () => new Text("SELF-CALL", 0, 0),
		async execute() { return { content: [{ type: "text", text: "done" }], details: {} }; },
	});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_autocomplete_custom_and_overlay_handles() {
    local(async {
        let ui = start(Options { files: vec![(".pi/extensions/gaps.ts", EXTENSION)], ..Options::default() }).await;
        ui.send("say %co").await;
        ui.wait_for("EXT-COMPLETION").await;
        ui.send("\t").await;
        ui.wait_until("completion applied", |ui| ui.screen().contains("say %color")).await;
        ui.send("\x03").await;

        ui.submit("/broken").await;
        ui.wait_for("custom rejected: factory boom").await;
        assert!(!ui.history().contains("custom resolved"));

        ui.submit("/overlay").await;
        ui.wait_for("overlay closed false true").await;
        assert!(!ui.screen().contains("OVERLAY-BODY"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_self_framed_tool_renderer_draws_without_the_box() {
    local(async {
        let responses = vec![FauxResponse::tool_call("call-1", "selfie", json!({})), FauxResponse::text("All done")];
        let files = vec![(".pi/extensions/gaps.ts", EXTENSION)];
        let ui = start(Options { responses, files, ..Options::default() }).await;
        ui.submit("draw").await;
        ui.wait_for("All done").await;
        let history = ui.history();
        assert!(history.lines().any(|line| line.starts_with("SELF-CALL")), "{history}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_of_a_session_whose_folder_is_gone_offers_the_current_folder() {
    local(async {
        let ui = start(Options::default()).await;
        let file = ui.agent_dir.parent().unwrap().join("moved.jsonl");
        let header = json!({"type": "session", "version": 3, "id": "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b",
            "timestamp": "2026-01-02T03:04:05.678Z", "cwd": "/nonexistent/pi-gone"});
        std::fs::write(&file, format!("{header}\n")).unwrap();
        ui.submit(&format!("/import {}", file.display())).await;
        ui.wait_for("Replace current session with").await;
        ui.send("\r").await;
        ui.wait_for("Session cwd not found").await;
        ui.send("\r").await;
        ui.wait_for("Session imported from:").await;
        assert_ne!(ui.session().cwd(), std::path::Path::new("/nonexistent/pi-gone"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_command_refreshes_catalogs_before_the_selector() {
    local(async {
        let ui = start(Options::default()).await;
        ui.submit("/model no-such-model").await;
        ui.wait_for("Only showing models from configured providers").await;
        let refreshed = |ui: &crate::harness::Ui| ui.screen().contains("Model catalogs refreshed.");
        ui.wait_until("selector refresh", refreshed).await;
        ui.send("\x1b").await;
        ui.wait_until("selector closed", |ui| !refreshed(ui)).await;
        ui.submit("/scoped-models").await;
        ui.wait_until("scoped models refresh", refreshed).await;
    })
    .await;
}
