//! An SDK `SettingsManager` shared with a child `AgentSession`: the child reads the caller's queued
//! changes, the caller's queue writes them once, and custom storage keeps working after the child.

#[path = "extensions/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use harness::{Setup, harness};
use pi_ai::providers::faux::FauxResponse;
use serde_json::{Value, json};

const PROBE: &str = r#"
import { existsSync, readFileSync } from 'node:fs';
import { createAgentSession, SessionManager, SettingsManager } from '@earendil-works/pi-coding-agent';

class LoggedStorage {
  constructor(global) { this.text = { global }; this.log = []; this.failWrites = null; }
  withLock(scope, fn) {
    const current = this.text[scope];
    const next = fn(current);
    if (next !== undefined && this.failWrites) throw this.failWrites;
    this.log.push({ scope, current: current ?? null, next: next ?? null });
    if (next !== undefined) this.text[scope] = next;
  }
}

const writes = (storage) => storage.log.filter((entry) => entry.next !== null).length;

async function child(ctx, settings) {
  const { session } = await createAgentSession({
    cwd: ctx.cwd, tools: [], settingsManager: settings, sessionManager: SessionManager.inMemory(ctx.cwd),
  });
  const seen = { model: session.model.id, thinkingLevel: session.thinkingLevel };
  await session.prompt('child question');
  session.dispose();
  return seen;
}

export default function (pi) {
  pi.registerCommand('probe', { handler: async (_args, ctx) => {
    const storage = new LoggedStorage(JSON.stringify({ defaultProvider: 'local', defaultModel: 'tiny', keep: 1 }, null, 2));
    const settings = SettingsManager.fromStorage(storage);
    settings.setDefaultModelAndProvider('local', 'other');
    settings.setDefaultThinkingLevel('high');
    const queuedBeforeChild = writes(storage);
    const custom = await child(ctx, settings);
    settings.setTheme('after-child');
    await settings.flush();
    const failure = new Error('custom write failed');
    storage.failWrites = failure;
    settings.setTheme('lost');
    await settings.flush();
    const [error] = settings.drainErrors();

    const dir = `${ctx.cwd}/sdk-agent`;
    const file = SettingsManager.create(ctx.cwd, dir);
    file.setDefaultModelAndProvider('local', 'tiny');
    file.setDefaultThinkingLevel('high');
    const before = existsSync(`${dir}/settings.json`);
    const fileChild = await child(ctx, file);
    await file.flush();
    pi.appendEntry('result', {
      queuedBeforeChild, custom, text: storage.text.global, log: storage.log,
      sameError: error?.error === failure, errorScope: error?.scope, errorPath: error && 'path' in error,
      theme: settings.getTheme(), fileChild, fileBeforeFlush: before,
      fileText: readFileSync(`${dir}/settings.json`, 'utf8'), lockLeft: existsSync(`${dir}/settings.json.lock`),
      fileErrors: file.drainErrors().length,
    });
  }});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_session_shares_the_sdk_settings_manager() {
    let responses = vec![FauxResponse::text("one"), FauxResponse::text("two")];
    let h = harness(Setup {
        files: vec![("probe.ts", PROBE.into())],
        load: vec!["probe.ts"],
        responses,
        ..Setup::default()
    })
    .await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    tokio::time::timeout(std::time::Duration::from_secs(30), h.session.prompt("/probe", Vec::new()))
        .await
        .unwrap()
        .unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entries = h.entries();
    let data = &entries.iter().find(|entry| entry["customType"] == "result").expect("result entry")["data"];

    assert_eq!(data["queuedBeforeChild"], 0, "setters write nothing before the queue runs");
    assert_eq!(data["custom"]["model"], "other", "the child reads the caller's queued model");
    let expected = json!({"defaultProvider": "local", "defaultModel": "other", "keep": 1,
        "defaultThinkingLevel": "high", "theme": "after-child"});
    let text: Value = serde_json::from_str(data["text"].as_str().unwrap()).unwrap();
    assert_eq!(serde_json::to_string(&text).unwrap(), serde_json::to_string(&expected).unwrap());
    let written: Vec<&Value> =
        data["log"].as_array().unwrap().iter().filter(|entry| !entry["next"].is_null()).collect();
    assert!(written.iter().all(|entry| entry["scope"] == "global"), "{written:?}");
    assert_eq!(written.last().map(|entry| &entry["next"]), Some(&data["text"]));

    assert_eq!(data["sameError"], true, "drainErrors returns the error the storage threw");
    assert_eq!((data["errorScope"].clone(), data["errorPath"].clone()), (json!("global"), json!(false)));
    assert_eq!(data["theme"], "lost", "a failed write keeps the value in memory");

    assert_eq!(data["fileChild"], json!({"model": "tiny", "thinkingLevel": "high"}));
    assert_eq!(data["fileBeforeFlush"], false, "the file is written by the queue, not by the setter");
    let file: Value = serde_json::from_str(data["fileText"].as_str().unwrap()).unwrap();
    assert_eq!(file, json!({"defaultProvider": "local", "defaultModel": "tiny", "defaultThinkingLevel": "high"}));
    assert_eq!(data["lockLeft"], false);
    assert_eq!(data["fileErrors"], 0);
}
