//! Upstream session, tree, and theme selectors and the login dialog in `ctx.ui.custom`, fed from
//! the host session store, session tree, theme registry, and keybindings.

use pi_ai::providers::faux::FauxResponse;

use crate::harness::{Options, Ui, local, start};

const EXTENSION: &str = r#"import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { LoginDialogComponent, SessionManager, SessionSelectorComponent, ThemeSelectorComponent, TreeSelectorComponent } from "@earendil-works/pi-coding-agent";

function writeSession(dir, cwd, id, text) {
	const header = { type: "session", version: 3, id, timestamp: "2026-01-01T00:00:00.000Z", cwd };
	const message = { type: "message", id: `${id}-m`, parentId: null, timestamp: "2026-01-01T00:00:01.000Z",
		message: { role: "user", content: text, timestamp: 1 } };
	writeFileSync(join(dir, `${id}.jsonl`), `${JSON.stringify(header)}\n${JSON.stringify(message)}\n`);
}

export default function (pi) {
	pi.registerCommand("sessions", { handler: async (_args, ctx) => {
		const dir = join(ctx.cwd, "stored-sessions");
		mkdirSync(dir, { recursive: true });
		writeSession(dir, ctx.cwd, "alpha", "alpha question");
		writeSession(dir, ctx.cwd, "beta", "beta question");
		const load = () => SessionManager.list(ctx.cwd, dir);
		const path = await ctx.ui.custom((tui, _theme, _keys, done) =>
			new SessionSelectorComponent(load, load, done, () => done(null), () => done(null), () => tui.requestRender()));
		ctx.ui.notify(`session ${path?.endsWith("beta.jsonl")}`);
	} });
	pi.registerCommand("pick-tree", { handler: async (_args, ctx) => {
		const manager = ctx.sessionManager;
		const id = await ctx.ui.custom((tui, _theme, _keys, done) =>
			new TreeSelectorComponent(manager.getTree(), manager.getLeafId(), tui.terminal.rows, done, () => done(null)));
		ctx.ui.notify(`tree ${manager.getEntry(id)?.message?.role}`);
	} });
	pi.registerCommand("pick-theme", { handler: async (_args, ctx) => {
		const previews = [];
		const name = await ctx.ui.custom((_tui, _theme, _keys, done) => {
			const selector = new ThemeSelectorComponent("dark", done, () => done(null), (preview) => previews.push(preview));
			// Upstream focuses the select list of this container; keys go to it the same way here.
			selector.handleInput = (data) => selector.getSelectList().handleInput(data);
			return selector;
		});
		ctx.ui.notify(`theme ${name} previews ${previews.join(",")}`);
	} });
	pi.registerCommand("auth-dialog", { handler: async (_args, ctx) => {
		const key = await ctx.ui.custom((tui, _theme, _keys, done) => {
			const dialog = new LoginDialogComponent(tui, "example", (ok, message) => done(`${ok} ${message}`), "Example");
			dialog.showPrompt("Enter API key", "sk-...").then(done, () => {});
			return dialog;
		});
		ctx.ui.notify(`login ${key}`);
	} });
}
"#;

async fn start_ui() -> Ui {
    let responses = vec![FauxResponse::text("first answer")];
    start(Options { files: vec![(".pi/extensions/components_b.ts", EXTENSION)], responses, ..Options::default() }).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_selector_component_searches_stored_sessions() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/sessions").await;
        ui.wait_until("sessions", |ui| ui.screen().contains("alpha question") && ui.screen().contains("beta question"))
            .await;
        ui.send("beta").await;
        ui.wait_until("filtered", |ui| !ui.screen().contains("alpha question")).await;
        ui.send("\r").await;
        ui.wait_for("session true").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_selector_component_selects_an_entry_of_the_session_tree() {
    local(async {
        let ui = start_ui().await;
        ui.submit("first question").await;
        ui.wait_for("first answer").await;
        ui.submit("/pick-tree").await;
        ui.wait_until("tree", |ui| ui.screen().contains("Session Tree") && ui.screen().contains("first question"))
            .await;
        ui.send("\x1b[A").await;
        ui.send("\r").await;
        ui.wait_for("tree user").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn theme_selector_component_lists_registry_themes_and_previews() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/pick-theme").await;
        ui.wait_until("themes", |ui| ui.screen().contains("light") && ui.screen().contains("dark")).await;
        ui.send("\x1b[B").await;
        ui.send("\r").await;
        // pi-rust ships more built-in themes (docs/parity/allowlist.json); Down from dark picks the next one.
        let names = pi_coding_agent::theme::get_available_themes();
        let next = &names[names.iter().position(|name| name == "dark").unwrap() + 1];
        ui.wait_for(&format!("theme {next} previews {next}")).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_dialog_component_returns_the_typed_secret() {
    local(async {
        let ui = start_ui().await;
        ui.submit("/auth-dialog").await;
        ui.wait_until("prompt", |ui| ui.screen().contains("Login to Example") && ui.screen().contains("Enter API key"))
            .await;
        ui.send("sk-test").await;
        ui.send("\r").await;
        ui.wait_for("login sk-test").await;
        ui.submit("/auth-dialog").await;
        ui.wait_until("prompt again", |ui| ui.screen().contains("Enter API key")).await;
        ui.send("\x1b").await;
        ui.wait_for("login false Login cancelled").await;
    })
    .await;
}
