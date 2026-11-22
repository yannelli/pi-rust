//! Extensions across session replacement and reload: `session_before_*` cancellation, shutdown
//! and start reasons, `ctx.newSession()` setup, `ctx.reload()`, and the no-UI context.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use pi_ai::auth::InMemoryCredentialStore;
use pi_ai::providers::faux::{FauxProvider, FauxResponse};
use pi_ai::register_api_provider;
use pi_coding_agent::agent_session::{AgentSessionRuntime, ForkPosition};
use pi_coding_agent::app::AppOptions;
use pi_coding_agent::extension_ui::{ExtensionMode, ExtensionUi, NoOpExtensionUi};
use pi_coding_agent::extensions::ExtensionBindings;
use pi_coding_agent::session::SessionManager;
use pi_coding_agent::settings::SettingsManager;
use serde_json::json;
use tempfile::TempDir;

use crate::extensions::harness::{RecordingUi, eventually};

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
        credential_env: Some(crate::offline_guard::credential_free_env()),
        ..AppOptions::default()
    };
    let manager = SessionManager::create(&cwd, Some(&agent_dir.join("sessions")));
    let runtime = Arc::new(crate::offline_guard::app_runtime(options, manager).await);
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
async fn session_manager_identity_follows_the_bound_session() {
    let source = r#"export default function (pi) {
	const started = new WeakMap();
	pi.on("session_start", (event, ctx) => started.set(ctx.sessionManager, event.reason));
	pi.registerCommand("owner", { handler: (_args, ctx) => {
		ctx.ui.notify(`started: ${started.get(ctx.sessionManager)}`);
	} });
	pi.registerCommand("replace", { handler: async (_args, ctx) => {
		const original = ctx.sessionManager;
		const originalId = original.getSessionId();
		await ctx.newSession({ withSession: async (next) => {
			const manager = next.sessionManager;
			const replaced = manager !== original && manager.getSessionId() !== originalId;
			let stale = false;
			try { void ctx.sessionManager; } catch (error) { stale = error.message.includes("stale"); }
			next.ui.notify(`replaced=${replaced}, stable=${manager === next.sessionManager}, stale=${stale}`);
		} });
	} });
}"#;
    let r = runtime(source, Vec::new(), ExtensionMode::Rpc).await;
    r.runtime.session().prompt("/owner", Vec::new()).await.unwrap();
    r.runtime.session().prompt("/replace", Vec::new()).await.unwrap();
    r.runtime.session().prompt("/owner", Vec::new()).await.unwrap();
    r.runtime.session().reload_extensions().await.unwrap();
    r.runtime.session().prompt("/owner", Vec::new()).await.unwrap();
    assert_eq!(
        r.ui.calls(),
        [
            "notify(info): started: startup",
            "notify(info): replaced=true, stable=true, stale=true",
            "notify(info): started: new",
            "notify(info): started: reload",
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn account_selection_writes_entries_and_scopes_auth_and_provider_overlays() {
    let source = r#"export default function (pi) {
        const started = new WeakSet();
        pi.on("session_start", (_, ctx) => started.add(ctx.modelRegistry.runtime));
        pi.registerCommand("activate", { handler: async (_, ctx) => {
            const registry = ctx.modelRegistry;
            const headers = { "X-Account": "test" };
            registry.registerProvider("local", { baseUrl: "http://selected.invalid", headers });
            const config = registry.getRegisteredProviderConfig("local");
            if (config.headers !== headers || !started.has(registry.runtime)) throw new Error("lost registry identity");
            await registry.runtime.setRuntimeApiKey("local", "synthetic-key");
            const model = registry.find("local", "tiny");
            const auth = await registry.getApiKeyAndHeaders(model);
            const header = Object.entries(auth.headers ?? {}).find(([key]) => key.toLowerCase() === "x-account");
            if (!auth.ok || auth.apiKey !== "synthetic-key" || header?.[1] !== "test" || model.baseUrl !== config.baseUrl)
                throw new Error("account override did not reach the request");
            let rejected = false;
            try { registry.registerProvider("local", { models: [{}] }); } catch { rejected = true; }
            if (!rejected || registry.getRegisteredProviderConfig("local") !== config) throw new Error("invalid update changed registration");
            const id = ctx.sessionManager.appendCustomEntry("account-selection", { account: "test" });
            if (!id || ctx.sessionManager.getEntry(id)?.data.account !== "test") throw new Error("selection did not persist");
            ctx.ui.notify(id);
        } });
        pi.registerCommand("clear-account", { handler: async (_, ctx) => {
            ctx.modelRegistry.unregisterProvider("local");
            await ctx.modelRegistry.runtime.removeRuntimeApiKey("local");
            if (ctx.modelRegistry.getRegisteredProviderConfig("local") !== undefined) throw new Error("config survived removal");
        } });
    }"#;
    let first = runtime(source, Vec::new(), ExtensionMode::Rpc).await;
    let other = runtime(source, Vec::new(), ExtensionMode::Rpc).await;
    let first_session = first.runtime.session();
    let other_session = other.runtime.session();
    first_session.prompt("/activate", Vec::new()).await.unwrap();
    let selection = first_session
        .session_manager()
        .entries()
        .iter()
        .map(entry_json)
        .find(|entry| entry["customType"] == "account-selection")
        .expect("account selection");
    assert_eq!(first.ui.calls(), vec![format!("notify(info): {}", selection["id"].as_str().unwrap())]);
    assert_eq!(first_session.model_runtime().api_key_for("local").await.as_deref(), Some("synthetic-key"));
    assert!(!other_session.model_runtime().has_runtime_api_key("local"));
    assert_eq!(other_session.model_runtime().get_model("local", "tiny").unwrap().base_url, "http://unused");
    first_session.prompt("/clear-account", Vec::new()).await.unwrap();
    assert!(!first_session.model_runtime().has_runtime_api_key("local"));
    assert_eq!(first_session.model_runtime().get_model("local", "tiny").unwrap().base_url, "http://unused");
}

#[tokio::test(flavor = "multi_thread")]
async fn account_radius_oauth_uses_the_bound_provider() {
    let source = r#"export default (pi) => {
        const credential = { type: "oauth", access: "sample", refresh: "sample-refresh", expires: 4102444800000 };
        pi.registerCommand("check-radius", { handler: async (_, ctx) => {
            const registry = ctx.modelRegistry;
            const flow = registry.getProvider("radius")?.auth.oauth;
            if (typeof flow?.login !== "function" || typeof flow.refresh !== "function") throw new Error("missing Radius OAuth");
            if ((await flow.toAuth(credential)).apiKey !== credential.access) throw new Error("wrong Radius conversion");
            if (registry.getProvider("absent-account-provider") !== undefined) throw new Error("unknown provider returned a flow");
            ctx.ui.notify("builtin Radius");
        } });
        pi.registerCommand("radius-overlay", { handler: async (_, ctx) => {
            const registry = ctx.modelRegistry;
            registry.registerProvider("radius", { oauth: {
                name: "Test Radius",
                login: async () => credential,
                refreshToken: async (item) => ({ ...item, access: "refreshed" }),
                getApiKey: (item) => "test-" + item.access,
            } });
            const oauth = registry.getProvider("radius").auth.oauth;
            const loggedIn = await oauth.login({ prompt: async () => "", notify: () => {} });
            const refreshed = await oauth.refresh(loggedIn);
            if (oauth.name !== "Test Radius" || (await oauth.toAuth(refreshed)).apiKey !== "test-refreshed")
                throw new Error("provider overlay did not handle OAuth");
            ctx.ui.notify("overlaid Radius");
        } });
        pi.registerCommand("clear-radius", { handler: (_, ctx) => ctx.modelRegistry.unregisterProvider("radius") });
    }"#;
    let first = runtime(source, Vec::new(), ExtensionMode::Rpc).await;
    let other = runtime(source, Vec::new(), ExtensionMode::Rpc).await;
    let session = first.runtime.session();
    session.prompt("/radius-overlay", Vec::new()).await.unwrap();
    other.runtime.session().prompt("/check-radius", Vec::new()).await.unwrap();
    session.prompt("/clear-radius", Vec::new()).await.unwrap();
    session.prompt("/check-radius", Vec::new()).await.unwrap();
    assert_eq!(first.ui.calls(), ["notify(info): overlaid Radius", "notify(info): builtin Radius"]);
    assert_eq!(other.ui.calls(), ["notify(info): builtin Radius"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn account_context_writes_follow_session_replacement() {
    let source = r#"export default function (pi) {
        pi.registerCommand("change", { handler: async (_, ctx) => {
            const old = ctx.modelRegistry, manager = ctx.sessionManager, oauth = old.getProvider("radius").auth.oauth;
            await ctx.newSession({ withSession: async (next) => {
                for (const write of [() => manager.appendCustomEntry("wrong-session"),
                    () => old.runtime.setRuntimeApiKey("local", "wrong-session"),
                    () => oauth.toAuth({ type: "oauth", access: "sample", refresh: "", expires: 1 }),
                    () => old.registerProvider("local", { baseUrl: "http://wrong.invalid" })]) {
                    try { await write(); } catch (error) { if (error.message.includes("stale")) continue; }
                    throw new Error("stale session accepted a write");
                }
                next.sessionManager.appendCustomEntry("after-replacement");
                await next.modelRegistry.runtime.setRuntimeApiKey("local", "replacement-key");
                next.modelRegistry.registerProvider("local", { baseUrl: "http://replacement.invalid" });
                if (next.modelRegistry.getRegisteredProviderConfig("local").baseUrl !== "http://replacement.invalid")
                    throw new Error("missing replacement registration");
                next.ui.notify("account selected");
            } });
        } });
        pi.registerCommand("check-account", { handler: async (_, ctx) => {
            const config = ctx.modelRegistry.getRegisteredProviderConfig("local");
            if (config?.baseUrl !== "http://replacement.invalid") throw new Error("worker did not retain registration");
            if (await ctx.modelRegistry.getApiKeyForProvider("local") !== "replacement-key") throw new Error("wrong key");
            ctx.ui.notify("selection active");
        } });
    }"#;
    let r = runtime(source, Vec::new(), ExtensionMode::Rpc).await;
    let original = r.runtime.session();
    original.prompt("/change", Vec::new()).await.unwrap();
    let current = r.runtime.session();
    current.prompt("/check-account", Vec::new()).await.unwrap();
    assert_eq!(r.ui.calls(), ["notify(info): account selected", "notify(info): selection active"]);
    assert!(
        !original.session_manager().entries().iter().any(|entry| entry_json(entry)["customType"] == "wrong-session")
    );
    assert!(
        current.session_manager().entries().iter().any(|entry| entry_json(entry)["customType"] == "after-replacement")
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
        credential_env: Some(crate::offline_guard::credential_free_env()),
        ..AppOptions::default()
    };
    let manager = SessionManager::create(&work, Some(&agent_dir.join("sessions")));
    let runtime = Arc::new(crate::offline_guard::app_runtime(options, manager).await);
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

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_new_session_setup_reports_the_extension_stack() {
    let source = r#"export default function (pi) {
	pi.registerCommand("broken", { handler: async (_args, ctx) => {
		await ctx.newSession({ setup: async () => { throw new Error("setup broke"); } });
	} });
}
"#;
    let r = runtime(source, vec![FauxResponse::text("ok")], ExtensionMode::Tui).await;
    r.runtime.session().prompt("/broken", Vec::new()).await.unwrap();
    let calls = r.ui.calls();
    let fatal = calls.iter().find(|call| call.starts_with("fatal: ")).expect("a fatal error");
    assert!(fatal.starts_with("fatal: Failed to create session: setup broke | "), "{fatal}");
    // Windows temp paths can be 8.3 short names while the JS stack shows the long form,
    // so match on the temp folder and file names.
    let folder = r.extension.parent().and_then(|dir| dir.file_name()).unwrap().to_string_lossy();
    assert!(fatal.contains(&*folder) && fatal.contains("ext.ts:"), "{fatal}");
}

/// pi-rust: `ctx.switchSession()` of `session_start` runs once the handlers finish.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_start_switch_runs_after_the_handlers_and_reports_the_cwd_change() {
    let target = tempfile::tempdir().unwrap();
    let other = target.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let file = target.path().join("other.jsonl");
    let header = json!({"type": "session", "version": 3, "id": "start-switch", "timestamp": "2025-01-01T00:00:00Z",
        "cwd": other.display().to_string()});
    std::fs::write(&file, format!("{header}\n")).unwrap();
    let path = serde_json::to_string(&file.display().to_string()).unwrap();
    let source = format!(
        r#"export default function (pi) {{
	pi.on("session_start", async (event, ctx) => {{
		ctx.ui.notify(`start ${{event.reason}}`);
		if (event.reason === "startup") ctx.ui.notify(`queued ${{await ctx.switchSession({path})}}`);
	}});
	pi.on("cwd_changed", (event, ctx) => ctx.ui.notify(`cwd ${{event.oldCwd}} -> ${{event.newCwd}}`));
}}
"#
    );
    let r = runtime(&source, Vec::new(), ExtensionMode::Tui).await;
    assert_eq!(r.runtime.session().session_file().as_deref(), Some(file.as_path()));
    eventually(|| r.ui.calls().len() >= 4).await;
    let work = r._root.path().join("work");
    assert_eq!(
        r.ui.calls(),
        vec![
            "notify(info): start startup".to_string(),
            "notify(info): queued undefined".into(),
            "notify(info): start resume".into(),
            format!("notify(info): cwd {} -> {}", work.display(), other.display()),
        ]
    );
}
