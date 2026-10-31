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

const WIDGETS: &str = r#"export default function (pi) {
	pi.on("session_start", (_event, ctx) => {
		for (const name of ["one", "two", "three"]) ctx.ui.setWidget(name, [`${name.toUpperCase()}-HEAD`, "a", "b", "c"]);
		ctx.ui.setWidget("four", ["FOUR-HEAD", "FOUR-BODY", "x", "y"], { placement: "belowEditor" });
	});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn widgets_over_a_third_of_the_rows_collapse_and_toggle() {
    local(async {
        let ui = start(Options { files: vec![(".pi/extensions/widgets.ts", WIDGETS)], ..Options::default() }).await;
        ui.wait_for("FOUR-HEAD +3 lines").await;
        assert!(ui.screen().contains("ONE-HEAD +3 lines"), "{}", ui.screen());
        assert!(!ui.screen().contains("FOUR-BODY"), "{}", ui.screen());

        ui.send("\x1bw").await;
        ui.wait_for("FOUR-BODY").await;
        assert!(!ui.screen().contains("ONE-HEAD +3"), "{}", ui.screen());

        ui.submit("/widgets collapse").await;
        ui.wait_until("collapsed", |ui| !ui.screen().contains("FOUR-BODY")).await;
        ui.submit("/widgets").await;
        ui.wait_until("full view", |ui| ui.screen().contains("FOUR-BODY")).await;
        assert!(ui.screen().contains("four"), "{}", ui.screen());
    })
    .await;
}

const STDERR: &str = r#"export default function (pi) {
	pi.registerCommand("errs", {
		description: "Write to stderr",
		handler: async () => {
			console.error("console %s", "error");
			console.warn("console warn");
			process.stderr.write("raw stderr\n");
		},
	});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_stderr_shows_as_warnings_in_the_chat() {
    local(async {
        let ui = start(Options { files: vec![(".pi/extensions/errs.ts", STDERR)], ..Options::default() }).await;
        ui.submit("/errs").await;
        ui.wait_for("Warning: [errs.ts] console error").await;
        ui.wait_for("Warning: [errs.ts] console warn").await;
        ui.wait_for("Warning: [errs.ts] raw stderr").await;
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_stderr_keeps_its_label_through_a_path_alias() {
    local(async {
        let root = tempfile::tempdir().unwrap();
        let agent_dir = root.path().join("agent");
        let extensions = agent_dir.join("extensions");
        std::fs::create_dir_all(&extensions).unwrap();
        let source = root.path().join("source.ts");
        std::fs::write(&source, STDERR).unwrap();
        std::os::unix::fs::symlink(source, extensions.join("errs.ts")).unwrap();
        let ui = start(Options { agent_dir: Some(agent_dir), ..Options::default() }).await;
        ui.submit("/errs").await;
        ui.wait_for("Warning: [errs.ts] console error").await;
        ui.wait_for("Warning: [errs.ts] console warn").await;
        ui.wait_for("Warning: [errs.ts] raw stderr").await;
    })
    .await;
}
