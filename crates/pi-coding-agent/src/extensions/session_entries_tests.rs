//! `ctx.sessionManager.getEntries()` copies each entry of the session into the extension runtime
//! once: Paseo calls it three times per turn, and a copy of a large session takes long.

use std::sync::{Arc, Mutex};

use pi_ai::auth::InMemoryCredentialStore;
use pi_ai::providers::faux::FauxProvider;
use pi_js::{HostFunction, HostValue, JsError};
use serde_json::{Value, json};

use crate::agent_session::{CreateAgentSessionOptions, CreateServicesOptions, create_agent_session};
use crate::agent_session::{create_agent_session_services, lock};
use crate::extension_ui::ExtensionMode;
use crate::extensions::{ExtensionBindings, LoadEntry, LoadOptions, load_extensions};
use crate::session::SessionManager;
use crate::settings::SettingsManager;

const EXTENSION: &str = r#"export default function (pi) {
	pi.registerCommand("fill", { handler: async (args) => {
		for (let index = 0; index < Number(args); index += 1) pi.appendEntry("bulk", { text: "x".repeat(1000) });
	} });
	pi.registerCommand("read", { handler: async (_args, ctx) => {
		const lengths = new Set();
		for (let call = 0; call < 20; call += 1) lengths.add(ctx.sessionManager.getEntries().length);
		pi.appendEntry("lengths", { lengths: [...lengths] });
	} });
}
"#;

/// Host calls of `getEntries()` and the entries they copied.
#[derive(Debug, Default, PartialEq)]
struct Reads {
    calls: usize,
    copied: usize,
}

fn entry_read(args: &[HostValue]) -> bool {
    let text = |index: usize| args.get(index).and_then(HostValue::as_str);
    text(0) == Some("session.read") && matches!(text(2), Some("entriesSince" | "getEntries"))
}

fn copied(result: &HostValue) -> usize {
    match result.as_json() {
        Some(Value::Array(entries)) => entries.len(),
        Some(value) => value["entries"].as_array().map_or(0, Vec::len),
        None => 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_get_entries_calls_copy_only_new_entries() {
    let root = tempfile::tempdir().unwrap();
    let (cwd, agent_dir) = (root.path().join("work"), root.path().join("agent"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    let extension = root.path().join("entries.ts");
    std::fs::write(&extension, EXTENSION).unwrap();
    let api = "faux-session-entries-cache";
    let models = json!({"providers": {"local": {"baseUrl": "http://unused", "api": api, "apiKey": "test-key",
        "models": [{"id": "tiny", "contextWindow": 128000, "maxTokens": 4000}]}}});
    std::fs::write(agent_dir.join("models.json"), models.to_string()).unwrap();
    pi_ai::register_api_provider(api, FauxProvider::new(Vec::new()));
    let settings = SettingsManager::in_memory(json!({"defaultProvider": "local", "defaultModel": "tiny"}));
    let services = create_agent_session_services(CreateServicesOptions {
        settings: Some(Arc::new(settings)),
        credentials: Some(Arc::new(InMemoryCredentialStore::new(Default::default()))),
        credential_env: Some(Arc::new(|_: &str| None::<String>)),
        ..CreateServicesOptions::new(&cwd, &agent_dir)
    })
    .await;
    let options = LoadOptions {
        entries: Some(vec![LoadEntry::file(extension)]),
        mode: Some(ExtensionMode::Tui),
        ..LoadOptions::default()
    };
    let runner = load_extensions(&services, &options).await.runner.expect("the extension loads");
    assert!(runner.load_errors().is_empty(), "{:?}", runner.load_errors());
    let mut create = CreateAgentSessionOptions::new(services, SessionManager::in_memory(&cwd));
    create.extensions = Some(runner.clone());
    let session = create_agent_session(create).await.unwrap().session;
    session.bind_extensions(ExtensionBindings::default()).await;

    // Count the reads at the host function the runtime calls, then run the real one.
    let reads = Arc::new(Mutex::new(Reads::default()));
    let (host, counted) = (Arc::downgrade(&runner), reads.clone());
    let counting = HostFunction::sync(move |args| {
        let runner = host.upgrade().ok_or_else(|| JsError::new("The extension host has shut down"))?;
        let read = entry_read(&args);
        let result = super::ops::dispatch_sync(&runner, args)?;
        if read {
            let mut reads = lock(&counted);
            reads.calls += 1;
            reads.copied += copied(&result);
        }
        Ok(result)
    });
    runner.js().register_host_function("ext.sync", counting);

    let lengths = |session: &crate::agent_session::AgentSession| {
        let manager = session.session_manager();
        let entry = manager.entries().iter().rev().find(|entry| entry.raw()["customType"] == "lengths").cloned();
        entry.map(|entry| entry.raw()["data"]["lengths"].clone())
    };
    session.prompt("/fill 200", Vec::new()).await.unwrap();
    let held = session.session_manager().entry_count();
    session.prompt("/read", Vec::new()).await.unwrap();
    assert_eq!(lengths(&session), Some(json!([held])));
    assert_eq!(
        *lock(&reads),
        Reads { calls: 20, copied: held },
        "the first call copies the session, later ones nothing"
    );

    session.prompt("/fill 50", Vec::new()).await.unwrap();
    let held = session.session_manager().entry_count();
    session.prompt("/read", Vec::new()).await.unwrap();
    assert_eq!(lengths(&session), Some(json!([held])));
    assert_eq!(*lock(&reads), Reads { calls: 40, copied: held }, "each entry is copied once");
}
