//! Extensions in the interactive mode: footer statuses, widgets around the editor, commands in
//! the slash menu, dialogs in the editor slot, message and entry renderers, and shortcuts.

use pi_ai::providers::faux::FauxResponse;

use crate::harness::{Options, Watchdog, local, start};

/// How long a native interactive test may run before the watchdog ends the test process.
const NATIVE_LIMIT: std::time::Duration = std::time::Duration::from_secs(90);

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

/// `package.json` of the native fixture, which pi runs as a child process.
fn native_package() -> &'static str {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native/fixture.mjs");
    let package = serde_json::json!({"name": "native-fixture", "pi": {"native": [{
        "command": env!("CARGO_BIN_EXE_pi"),
        "args": [fixture.display().to_string()],
        "env": {"PI_JS_FORK": "1"},
    }]}});
    Box::leak(package.to_string().into_boxed_str())
}

/// The render width of the first visible line with `prefix`: the number after its last ` at `.
fn drawn_width(screen: &str, prefix: &str) -> Option<usize> {
    let line = screen.lines().find(|line| line.contains(prefix))?;
    line.rsplit(" at ").next()?.split_whitespace().next()?.parse().ok()
}

#[test]
fn native_renderers_draw_tool_rows_messages_and_entries_in_the_transcript() {
    let dog = Watchdog::start("native_renderers_draw_tool_rows_messages_and_entries_in_the_transcript", NATIVE_LIMIT);
    dog.stage("build Tokio runtime");
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let package = native_package();
    runtime.block_on(local(async {
        dog.stage("start");
        let ui = start(Options {
            files: vec![(".pi/extensions/native/package.json", package)],
            responses: vec![
                FauxResponse::tool_call("c1", "native_draw", serde_json::json!({"text": 7})),
                FauxResponse::text("drawn"),
            ],
            ..Options::default()
        })
        .await;
        dog.stage("submit the tool call");
        ui.submit("draw seven").await;
        dog.stage("wait for the result row");
        ui.wait_for("RESULT drew coerced 7 at").await;
        dog.stage("wait for the call row");
        ui.wait_for("CALL ").await;
        let wide = drawn_width(&ui.screen(), "CALL ").expect("a call row with its width");
        assert!((80..100).contains(&wide), "call width {wide}:\n{}", ui.screen());
        dog.stage("resize to 70 columns");
        ui.terminal.borrow_mut().resize(70, 30);
        ui.wait_until("a narrower call row", |ui| drawn_width(&ui.screen(), "CALL ").is_some_and(|width| width < 70))
            .await;
        dog.stage("post the card and the note");
        ui.submit("/native-card").await;
        ui.wait_for("CARD teal card at").await;
        ui.wait_for("NOTE plum at").await;
        let screen = ui.screen();
        assert_eq!(drawn_width(&screen, "CARD teal card at"), Some(70), "{screen}");
        assert!(screen.contains("NOTE plum at 70 closed"), "{screen}");
        dog.stage("tear down");
    }));
    dog.stage("drop Tokio runtime");
    drop(runtime);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_native_panel_repaints_after_a_height_only_resize() {
    let package = native_package();
    let _dog = Watchdog::start("a_native_panel_repaints_after_a_height_only_resize", NATIVE_LIMIT);
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

fn entry_type(entry: &pi_coding_agent::session::Entry) -> Option<&str> {
    entry.raw().get("customType").and_then(|kind| kind.as_str())
}

/// The core `anthropic` login of the test: no browser and no request.
struct FakeAnthropic;

#[async_trait::async_trait]
impl pi_ai::auth::OAuthAuth for FakeAnthropic {
    fn name(&self) -> &str {
        "Anthropic (Claude Pro/Max)"
    }

    async fn login(
        &self,
        _: &dyn pi_ai::auth::AuthInteraction,
        _: &pi_ai::auth::LoginOptions,
        _: tokio_util::sync::CancellationToken,
    ) -> pi_ai::auth::AuthResult<pi_ai::auth::OAuthCredential> {
        let expires = pi_ai::now_ms() + 3_600_000;
        Ok(pi_ai::auth::OAuthCredential {
            refresh: "r".into(),
            access: "slot-access".into(),
            expires,
            extra: Default::default(),
        })
    }

    async fn refresh(
        &self,
        credential: &pi_ai::auth::OAuthCredential,
        _: tokio_util::sync::CancellationToken,
    ) -> pi_ai::auth::AuthResult<pi_ai::auth::OAuthCredential> {
        Ok(credential.clone())
    }

    fn to_auth(&self, credential: &pi_ai::auth::OAuthCredential) -> pi_ai::auth::ModelAuth {
        pi_ai::auth::ModelAuth { api_key: Some(credential.access.clone()), ..Default::default() }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_auth_login_runs_the_login_dialog_of_pi() {
    let package = native_package();
    let _dog = Watchdog::start("native_auth_login_runs_the_login_dialog_of_pi", NATIVE_LIMIT);
    let oauth: pi_ai::auth::OAuthLookup = std::sync::Arc::new(|id: &str| {
        (id == "anthropic").then(|| std::sync::Arc::new(FakeAnthropic) as std::sync::Arc<dyn pi_ai::auth::OAuthAuth>)
    });
    local(async {
        let files = vec![(".pi/extensions/native/package.json", package)];
        let ui = start(Options { files, oauth: Some(oauth), ..Options::default() }).await;
        ui.submit("/native-oauth").await;
        ui.submit("/native-login").await;
        let login = |ui: &crate::harness::Ui| {
            let session = ui.session();
            let manager = session.session_manager();
            let entries = manager.entries();
            let kind = |entry: &&_| entry_type(entry) == Some("native-login");
            entries.iter().find(kind).and_then(|entry| entry.raw().get("data").cloned())
        };
        ui.wait_until("the login result", |ui| login(ui).is_some()).await;
        let data = login(&ui).unwrap();
        assert_eq!(data["slot"], serde_json::json!({"result": true}), "{data}");
        assert_eq!(data["foreign"]["code"], serde_json::json!(-32602), "{data}");
        let store = ui.session().model_runtime().credential_store();
        let stored = store.read("anthropic-2").await.unwrap();
        assert!(
            matches!(stored, Some(pi_ai::auth::Credential::OAuth(ref credential)) if credential.access == "slot-access"),
            "{stored:?}"
        );
        assert!(store.read("anthropic").await.unwrap().is_none());
    })
    .await;
}
