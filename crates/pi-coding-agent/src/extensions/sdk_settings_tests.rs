//! A native settings write by a child session reaches the extension's custom storage through the
//! JavaScript drain callback while the extension command still waits, with no JavaScript call between.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pi_ai::ThinkingLevel;
use pi_ai::auth::InMemoryCredentialStore;
use pi_ai::providers::faux::FauxProvider;
use pi_js::JsBackend;
use serde_json::{Value, json};

use crate::agent_session::{
    CreateAgentSessionOptions, CreateServicesOptions, create_agent_session, create_agent_session_services, lock,
};
use crate::extension_ui::ExtensionMode;
use crate::extensions::backend::{ExtensionRuntime, RUNTIME_ENV};
use crate::extensions::{ExtensionBindings, LoadEntry, LoadOptions, load_extensions};
use crate::session::SessionManager;
use crate::settings::SettingsManager;

const PROBE: &str = r#"
import { writeFileSync } from 'node:fs';
import { createAgentSession, SessionManager, SettingsManager } from '@earendil-works/pi-coding-agent';
export default function (pi) {
  pi.registerCommand('probe', { handler: async (_args, ctx) => {
    const log = [];
    let written;
    const done = new Promise((resolve) => { written = resolve; });
    const storage = {
      text: JSON.stringify({ keep: 1 }, null, 2),
      withLock(scope, fn) {
        const current = scope === 'global' ? this.text : undefined;
        const next = fn(current);
        log.push({ scope, current: current ?? null, next: next ?? null });
        if (next !== undefined && scope === 'global') {
          this.text = next;
          written();
        }
      },
    };
    const settings = SettingsManager.fromStorage(storage);
    const { session } = await createAgentSession({
      cwd: ctx.cwd, tools: [], settingsManager: settings, sessionManager: SessionManager.inMemory(ctx.cwd),
    });
    const before = log.length;
    writeFileSync(`${ctx.cwd}/ready`, '');
    await done;
    const result = { before, log, text: storage.text, childLevel: session.thinkingLevel,
      level: settings.getDefaultThinkingLevel(), errors: settings.drainErrors().length };
    await settings.flush();
    result.writes = log.filter((entry) => entry.next !== null).length;
    session.dispose();
    writeFileSync(`${ctx.cwd}/result.json`, JSON.stringify(result));
  }});
}
"#;

fn write_models(agent_dir: &Path, api: &str) {
    let models = json!({"providers": {"local": {"baseUrl": "http://unused", "api": api, "apiKey": "test-key",
        "models": [{"id": "tiny", "contextWindow": 128000, "maxTokens": 4000, "reasoning": true, "input": ["text"]}]}}});
    std::fs::write(agent_dir.join("models.json"), models.to_string()).unwrap();
}

async fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {}", path.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_write_reaches_custom_storage_through_the_drain_callback() {
    let root = tempfile::tempdir().unwrap();
    let (cwd, agent_dir, extensions) = (root.path().join("work"), root.path().join("agent"), root.path().join("ext"));
    for dir in [&cwd, &agent_dir, &extensions] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(extensions.join("probe.ts"), PROBE).unwrap();
    let api = "faux-sdk-settings-drain";
    write_models(&agent_dir, api);
    pi_ai::register_api_provider(api, FauxProvider::new(Vec::new()));
    let host_settings = SettingsManager::in_memory(json!({"defaultProvider": "local", "defaultModel": "tiny"}));
    let services = create_agent_session_services(CreateServicesOptions {
        settings: Some(Arc::new(host_settings)),
        credentials: Some(Arc::new(InMemoryCredentialStore::new(Default::default()))),
        credential_env: Some(Arc::new(|_: &str| None::<String>)),
        ..CreateServicesOptions::new(&cwd, &agent_dir)
    })
    .await;
    let options = LoadOptions {
        entries: Some(vec![LoadEntry::file(extensions.join("probe.ts"))]),
        mode: Some(ExtensionMode::Tui),
        ..LoadOptions::default()
    };
    let runner = load_extensions(&services, &options).await.runner.expect("the probe loads");
    let requested = std::env::var(RUNTIME_ENV).ok().and_then(|value| ExtensionRuntime::parse(&value));
    if requested == Some(ExtensionRuntime::Node) {
        assert!(matches!(runner.backend(), JsBackend::Node { .. }), "Node backend not started");
    }
    assert!(runner.load_errors().is_empty(), "{:?}", runner.load_errors());
    let mut create = CreateAgentSessionOptions::new(services, SessionManager::in_memory(&cwd));
    create.extensions = Some(runner.clone());
    let session = create_agent_session(create).await.unwrap().session;
    session.bind_extensions(ExtensionBindings::default()).await;

    let child_write = async {
        wait_for(&cwd.join("ready")).await;
        let child = lock(&runner.sdk.children).values().next().map(|child| child.session.clone());
        child.expect("the child session").set_thinking_level(ThinkingLevel::High, true);
    };
    let both = async { tokio::join!(session.prompt("/probe", Vec::new()), child_write) };
    let (prompted, ()) = tokio::time::timeout(Duration::from_secs(30), both).await.expect("the drain callback ran");
    prompted.unwrap();

    let result: Value = serde_json::from_str(&std::fs::read_to_string(cwd.join("result.json")).unwrap()).unwrap();
    assert_eq!(result["before"], 2, "only the two loads ran before the child wrote");
    let last = &result["log"][2];
    assert_eq!((last["scope"].clone(), last["current"].clone()), (json!("global"), json!("{\n  \"keep\": 1\n}")));
    assert_eq!(result["text"], "{\n  \"keep\": 1,\n  \"defaultThinkingLevel\": \"high\"\n}");
    assert_eq!((result["childLevel"].clone(), result["level"].clone()), (json!("high"), json!("high")));
    assert_eq!((result["errors"].clone(), result["writes"].clone()), (json!(0), json!(1)));
}
