//! Extensions across session replacement and reload: `session_before_*` cancellation, shutdown
//! and start reasons, `ctx.newSession()` setup, `ctx.reload()`, and the no-UI context.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use pi_ai::auth::InMemoryCredentialStore;
use pi_ai::providers::faux::{FauxProvider, FauxResponse};
use pi_ai::register_api_provider;
use pi_coding_agent::agent_session::{AgentSessionRuntime, ForkPosition};
use pi_coding_agent::app::{AppOptions, create_app_runtime};
use pi_coding_agent::extension_ui::{ExtensionMode, ExtensionUi, NoOpExtensionUi};
use pi_coding_agent::extensions::ExtensionBindings;
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::settings::SettingsManager;
use serde_json::json;
use tempfile::TempDir;

use crate::harness::{RecordingUi, eventually};

struct Runtime {
    _root: TempDir,
    runtime: Arc<AgentSessionRuntime>,
    ui: Arc<RecordingUi>,
    extension: PathBuf,
}

/// A runtime whose sessions load `source` from the settings, bound to a recording UI.
async fn runtime(source: &str, responses: Vec<FauxResponse>, mode: ExtensionMode) -> Runtime {
    runtime_with(source, FauxProvider::new(responses), mode).await
}

async fn runtime_with(source: &str, provider: Arc<FauxProvider>, mode: ExtensionMode) -> Runtime {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let root = tempfile::tempdir().unwrap();
    let (cwd, agent_dir) = (root.path().join("work"), root.path().join("agent"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    let extension = root.path().join("ext.ts");
    std::fs::write(&extension, source).unwrap();
    let api = format!("faux-runtime-{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let models = json!({"providers": {"local": {"baseUrl": "http://unused", "api": api, "apiKey": "k",
        "models": [{"id": "tiny", "contextWindow": 128000, "maxTokens": 4000}]}}});
    std::fs::write(agent_dir.join("models.json"), models.to_string()).unwrap();
    register_api_provider(api, provider);
    let settings = json!({"defaultProvider": "local", "defaultModel": "tiny",
        "extensions": [extension.display().to_string()]});
    let options = AppOptions {
        agent_dir: agent_dir.clone(),
        settings: Some(Arc::new(SettingsManager::in_memory(settings))),
        credentials: Some(Arc::new(InMemoryCredentialStore::new(BTreeMap::new()))),
        ..AppOptions::default()
    };
    let manager = SessionManager::create(&cwd, Some(&agent_dir.join("sessions")));
    let runtime = Arc::new(create_app_runtime(options, manager).await.unwrap());
    let ui = Arc::new(RecordingUi::default());
    // Modes without a UI bind the no-op UI, as print mode does.
    let bound: Arc<dyn ExtensionUi> = if mode.has_ui() { ui.clone() } else { Arc::new(NoOpExtensionUi) };
    let bindings =
        ExtensionBindings { ui: bound, mode, runtime: Some(Arc::downgrade(&runtime)), ..ExtensionBindings::default() };
    runtime.session().bind_extensions(bindings).await;
    Runtime { _root: root, runtime, ui, extension }
}

fn entry_json(entry: &pi_coding_agent::session::entry::Entry) -> serde_json::Value {
    serde_json::Value::Object(entry.raw().clone())
}

const LIFECYCLE: &str = r#"export default function (pi) {
	pi.on("session_start", (event, ctx) => ctx.ui.notify(`start ${event.reason}${event.previousSessionFile ? " +previous" : ""}`));
	pi.on("session_shutdown", (event, ctx) => ctx.ui.notify(`shutdown ${event.reason}`));
	pi.on("session_before_switch", (event, ctx) => {
		ctx.ui.notify(`before_switch ${event.reason}`);
		return { cancel: event.reason === "resume" };
	});
	pi.on("session_before_fork", (event, ctx) => {
		ctx.ui.notify(`before_fork ${event.position}`);
		return { cancel: true };
	});
	pi.registerCommand("fresh", { handler: async (_args, ctx) => {
		// The command's ctx is stale once the session is replaced, as upstream.
		await ctx.newSession({ setup: async (sm) => { sm.appendCustomEntry("seed", { from: "setup" }); } });
	} });
	pi.registerCommand("again", { handler: async (_args, ctx) => { await ctx.reload(); } });
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn replacing_sessions_runs_the_lifecycle_and_honors_cancellation() {
    let r = runtime(LIFECYCLE, vec![FauxResponse::text("ok")], ExtensionMode::Tui).await;
    assert_eq!(r.ui.calls(), vec!["notify(info): start startup"]);
    r.runtime.session().prompt("hello", Vec::new()).await.unwrap();
    let file = r.runtime.session().session_file().expect("persisted");

    let switched = r.runtime.switch_session(&file, None).await.unwrap();
    assert!(switched.cancelled);
    let user_entry = {
        let session = r.runtime.session();
        let manager = session.session_manager();
        let entry = manager.entries().iter().find(|entry| entry_json(entry)["message"]["role"] == "user").cloned();
        entry.expect("user entry").id().to_string()
    };
    let forked = r.runtime.fork(&user_entry, ForkPosition::Before).await.unwrap();
    assert!(forked.cancelled);

    r.runtime.session().prompt("/fresh", Vec::new()).await.unwrap();
    let seeded =
        r.runtime.session().session_manager().entries().iter().any(|entry| entry_json(entry)["customType"] == "seed");
    assert!(seeded, "the setup callback wrote to the new session");
    assert_eq!(
        r.ui.calls(),
        vec![
            "notify(info): start startup",
            "notify(info): before_switch resume",
            "notify(info): before_fork before",
            "notify(info): before_switch new",
            "notify(info): shutdown new",
            "notify(info): start new +previous",
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_loads_changed_extensions_and_starts_them_again() {
    let r = runtime(LIFECYCLE, Vec::new(), ExtensionMode::Tui).await;
    let changed = LIFECYCLE.replace("start ${event.reason}", "restarted ${event.reason}");
    std::fs::write(&r.extension, changed).unwrap();
    r.runtime.session().prompt("/again", Vec::new()).await.unwrap();
    eventually(|| r.ui.calls().len() >= 3).await;
    assert_eq!(
        r.ui.calls(),
        vec!["notify(info): start startup", "notify(info): shutdown reload", "notify(info): restarted reload"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_ui_dialogs_return_their_defaults() {
    let source = r#"export default function (pi) {
	pi.registerCommand("ask", { handler: async (_args, ctx) => {
		const choice = await ctx.ui.select("Pick", ["a", "b"]);
		const ok = await ctx.ui.confirm("Sure?", "Really");
		const text = await ctx.ui.input("Name");
		pi.appendEntry("answers", { mode: ctx.mode, hasUI: ctx.hasUI, choice: choice ?? null, ok, text: text ?? null });
	} });
}
"#;
    let r = runtime(source, Vec::new(), ExtensionMode::Print).await;
    r.runtime.session().prompt("/ask", Vec::new()).await.unwrap();
    let entries: Vec<_> = r.runtime.session().session_manager().entries().iter().map(entry_json).collect();
    let answers = entries.iter().find(|entry| entry["customType"] == "answers").expect("answers entry");
    assert_eq!(answers["data"], json!({"mode": "print", "hasUI": false, "choice": null, "ok": false, "text": null}));
    assert!(r.ui.calls().is_empty(), "{:?}", r.ui.calls());
}

const SHUTDOWN_ROLES: &str = r#"export default function (pi) {
	pi.on("session_shutdown", (event, ctx) => {
		const roles = ctx.sessionManager.getEntries().filter((e) => e.type === "message").map((e) => e.message.role);
		ctx.ui.notify(`shutdown ${event.reason} ${roles.join(",")} idle=${ctx.isIdle()}`);
	});
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn the_running_turn_is_aborted_and_saved_before_session_shutdown() {
    let long = "word ".repeat(400);
    let provider = FauxProvider::with_delay(vec![FauxResponse::text(long)], std::time::Duration::from_millis(20));
    let r = runtime_with(SHUTDOWN_ROLES, provider, ExtensionMode::Tui).await;
    let session = r.runtime.session();
    let prompt = tokio::spawn(async move { session.prompt("hello", Vec::new()).await });
    let streaming = r.runtime.session();
    eventually(|| streaming.is_streaming()).await;
    let old_file = r.runtime.session().session_file().expect("persisted");
    r.runtime.new_session(None, None).await.unwrap();
    let _ = prompt.await;
    let calls = r.ui.calls();
    let shutdown = calls.iter().find(|call| call.starts_with("notify(info): shutdown new")).expect("shutdown");
    assert!(shutdown.ends_with("user,assistant idle=true"), "{calls:?}");
    let saved = std::fs::read_to_string(old_file).unwrap();
    assert!(saved.contains("\"stopReason\":\"aborted\""), "{saved}");
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_gives_project_trust_handlers_the_live_ui() {
    let root = tempfile::tempdir().unwrap();
    let (work, other, agent_dir) = (root.path().join("work"), root.path().join("other"), root.path().join("agent"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(other.join(".pi/extensions")).unwrap();
    std::fs::write(other.join(".pi/extensions/project.ts"), "export default function () {}\n").unwrap();
    std::fs::create_dir_all(agent_dir.join("extensions")).unwrap();
    let source = r#"export default function (pi) {
	pi.on("project_trust", async (event, ctx) => {
		const choice = await ctx.ui.select(`Trust ${event.cwd}? (${ctx.mode}, ${ctx.hasUI})`, ["Trust", "Skip"]);
		return choice === "Trust" ? { trusted: "yes", remember: true } : undefined;
	});
}
"#;
    std::fs::write(agent_dir.join("extensions/trust.ts"), source).unwrap();
    let api = "faux-runtime-resume-trust";
    let models = json!({"providers": {"local": {"baseUrl": "http://unused", "api": api, "apiKey": "k",
        "models": [{"id": "tiny", "contextWindow": 128000, "maxTokens": 4000}]}}});
    std::fs::write(agent_dir.join("models.json"), models.to_string()).unwrap();
    let settings = json!({"defaultProvider": "local", "defaultModel": "tiny"});
    std::fs::write(agent_dir.join("settings.json"), settings.to_string()).unwrap();
    register_api_provider(api, FauxProvider::new(Vec::new()));
    let options = AppOptions {
        agent_dir: agent_dir.clone(),
        credentials: Some(Arc::new(InMemoryCredentialStore::new(BTreeMap::new()))),
        ..AppOptions::default()
    };
    let manager = SessionManager::create(&work, Some(&agent_dir.join("sessions")));
    let runtime = Arc::new(create_app_runtime(options, manager).await.unwrap());
    let ui = Arc::new(RecordingUi::default());
    ui.answers.lock().unwrap().push("Trust".into());
    let bindings = ExtensionBindings {
        ui: ui.clone(),
        mode: ExtensionMode::Tui,
        runtime: Some(Arc::downgrade(&runtime)),
        ..ExtensionBindings::default()
    };
    runtime.session().bind_extensions(bindings).await;

    let file = root.path().join("other.jsonl");
    let header = json!({"type": "session", "version": 3, "id": "resume-trust", "timestamp": "2025-01-01T00:00:00Z",
        "cwd": other.display().to_string()});
    std::fs::write(&file, format!("{header}\n")).unwrap();
    let prompt: pi_coding_agent::agent_session::TrustPromptFn = Arc::new(|_| Box::pin(async { None }));
    runtime.switch_session_with(&file, None, Some(prompt)).await.unwrap();
    let expected = format!("select: Trust {}? (tui, true) [Trust, Skip]", other.display());
    assert!(ui.calls().contains(&expected), "{:?}", ui.calls());
    let store = pi_coding_agent::resources::trust::ProjectTrustStore::new(&agent_dir);
    assert_eq!(store.get(&other).unwrap(), Some(true));
}
