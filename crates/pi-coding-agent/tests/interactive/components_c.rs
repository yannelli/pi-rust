//! Upstream extension editor, footer, model and settings selectors in the Rust terminal: the editor
//! runs the external editor with the TUI stopped, and the others read real host sessions and settings.

use crate::harness::{Options, Ui, local, start};

const EXTENSION: &str = r#"import { createAgentSession, ExtensionEditorComponent, FooterComponent, ModelRuntime, ModelSelectorComponent, SettingsManager, SettingsSelectorComponent } from "@earendil-works/pi-coding-agent";

function settingsConfig(autoCompact) {
	return {
		autoCompact, defaultModel: "", availableDefaultModels: [], showImages: true, imageWidthCells: 60, autoResizeImages: true,
		blockImages: false, enableSkillCommands: true, steeringMode: "one-at-a-time", followUpMode: "one-at-a-time",
		transport: "auto", sessionShareDestination: "auto", httpIdleTimeoutMs: 300000, cacheWarmingMode: "off",
		thinkingLevel: "off", availableThinkingLevels: ["off"], modelThinkingLevels: {}, currentTheme: "dark",
		terminalTheme: "dark", availableThemes: ["dark", "light"], hideThinkingBlock: false, mermaidRenderingMode: "auto",
		showCacheMissNotices: true, collapseChangelog: false, enableInstallTelemetry: true, doubleEscapeAction: "tree",
		treeFilterMode: "default", showHardwareCursor: false, editorPaddingX: 0, outputPad: 1, autocompleteMaxVisible: 5,
		quietStartup: false, defaultProjectTrust: "ask", clearOnShrink: false, showTerminalProgress: true, tuiMode: "main",
		fullscreenExitOutput: "transcript", fullscreenScrollbar: "auto", fullscreenCopyOnSelect: true,
		fullscreenWheelScrollLines: 3, warnings: {},
	};
}

export default function (pi) {
	pi.registerCommand("edit-note", { handler: async (args, ctx) => {
		const text = await ctx.ui.custom((tui, _theme, keys, done) =>
			new ExtensionEditorComponent(tui, keys, "Edit note", "draft", done, () => done("cancelled"), undefined, args.trim()));
		ctx.ui.notify(`edited ${text}`);
	} });
	pi.registerCommand("show-footer", { handler: async (_args, ctx) => {
		const session = await createAgentSession({ cwd: ctx.cwd, model: ctx.model });
		ctx.ui.setFooter((_tui, _theme, footerData) => new FooterComponent(session, footerData));
		ctx.ui.notify(`footer ${session.state.model.id}`);
	} });
	pi.registerCommand("pick-model", { handler: async (_args, ctx) => {
		const runtime = await ModelRuntime.create();
		const picked = await ctx.ui.custom((tui, _theme, _keys, done) =>
			new ModelSelectorComponent(tui, undefined, runtime, [], done, () => done(null)));
		ctx.ui.notify(`picked ${picked?.provider}/${picked?.id}`);
	} });
	pi.registerCommand("edit-settings", { handler: async (_args, ctx) => {
		const settings = SettingsManager.create(ctx.cwd);
		const callbacks = new Proxy({}, { get: (target, name) => target[name] ?? (() => {}) });
		const result = await ctx.ui.custom((_tui, _theme, _keys, done) => {
			const selector = new SettingsSelectorComponent(
				settingsConfig(settings.getCompactionEnabled()),
				Object.assign(callbacks, {
					onAutoCompactChange: (enabled) => settings.setCompactionEnabled(enabled),
					onCancel: () => done(settings.getCompactionEnabled()),
				}),
			);
			// Upstream focuses the settings list of this container; keys go to it the same way here.
			selector.handleInput = (data) => selector.getSettingsList().handleInput(data);
			return selector;
		});
		ctx.ui.notify(`autocompact ${result}`);
	} });
}
"#;

async fn start_ui() -> Ui {
    start(Options { files: vec![(".pi/extensions/components_c.ts", EXTENSION)], ..Options::default() }).await
}

/// An executable `sh` script in `dir` that runs `body` with the edited file as `$1`.
#[cfg(unix)]
fn editor_script(dir: &std::path::Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_editor_component_runs_the_external_editor_and_resumes() {
    local(async {
        let ui = start_ui().await;
        let scripts = tempfile::tempdir().unwrap();
        let writes = editor_script(scripts.path(), "writes.sh", "printf 'edited by script' > \"$1\"");
        let fails = editor_script(scripts.path(), "fails.sh", "exit 3");

        ui.submit(&format!("/edit-note {writes}")).await;
        ui.wait_until("editor", |ui| ui.screen().contains("Edit note") && ui.screen().contains("draft")).await;
        ui.send("\x07").await;
        ui.wait_until("edited text", |ui| ui.screen().contains("edited by script")).await;
        ui.send("\r").await;
        ui.wait_for("edited edited by script").await;

        ui.submit(&format!("/edit-note {fails}")).await;
        ui.wait_until("editor again", |ui| ui.screen().contains("Edit note")).await;
        ui.send("\x07").await;
        ui.wait_for("External editor exited with code 3; prompt unchanged").await;
        ui.send("\x1b").await;
        ui.wait_for("edited cancelled").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn footer_component_shows_a_child_session_model_and_context() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/show-footer").await;
        ui.wait_for("footer ").await;
        let model = ui.screen().split("footer ").nth(1).and_then(|rest| rest.split_whitespace().next()).unwrap().to_string();
        ui.wait_until("footer model", |ui| ui.screen().matches(model.as_str()).count() >= 2).await;
        assert!(ui.screen().contains("%/"), "{}", ui.screen());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_selector_component_picks_from_the_host_runtime() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/pick-model").await;
        ui.wait_until("models", |ui| ui.screen().contains("Only showing models")).await;
        ui.send("\r").await;
        ui.wait_until("picked", |ui| ui.screen().contains("picked ") && !ui.screen().contains("picked undefined")).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_selector_component_writes_through_settings_manager() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/edit-settings").await;
        ui.wait_until("settings", |ui| ui.screen().contains("Auto-compact")).await;
        ui.send("\r").await;
        ui.send("\x1b").await;
        ui.wait_for("autocompact false").await;
    })
    .await;
}
