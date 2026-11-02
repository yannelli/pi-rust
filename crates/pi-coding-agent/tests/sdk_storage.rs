//! SDK storage exports for extensions: `SessionManager` JSONL round trips, `SettingsManager` file
//! reads and writes, `DefaultResourceLoader` system prompts, and the host `ModelRuntime`.

#[path = "extensions/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "agent_session/mcp_fake.rs"]
#[allow(dead_code)]
mod mcp_fake;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use harness::{Harness, Setup, harness};
use pi_coding_agent::session::SessionManager;
use serde_json::{Value, json};

async fn run_command(source: &str, before: impl FnOnce(&Harness)) -> (Harness, Value) {
    let h =
        harness(Setup { files: vec![("probe.ts", source.into())], load: vec!["probe.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    before(&h);
    tokio::time::timeout(std::time::Duration::from_secs(20), h.session.prompt("/probe", Vec::new()))
        .await
        .unwrap()
        .unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entries = h.entries();
    let result = entries.iter().find(|entry| entry["customType"] == "result").expect("result entry");
    let data = result["data"].clone();
    (h, data)
}

const SESSIONS: &str = r#"
import { SessionManager } from '@earendil-works/pi-coding-agent';
const code = (run) => { try { run(); return null; } catch (error) { return error.code ?? error.message; } };
const FULL_USAGE = { input: 3, output: 4, cacheRead: 5, cacheWrite: 6, totalTokens: 18,
  cost: { input: 0.5, output: 0.25, cacheRead: 0, cacheWrite: 0, total: 0.75 } };
const EXTENDED_USAGE = { input: 1.5, output: 2, extra: { nested: [1, 'two'], flag: true } };
export default function (pi) {
  pi.registerCommand('probe', { handler: async (_args, ctx) => {
    const dir = ctx.cwd + '/sdk-sessions';
    const manager = SessionManager.create(ctx.cwd, dir);
    manager.appendModelChange('local', 'tiny');
    manager.appendThinkingLevelChange('low');
    const user = manager.appendMessage({ role: 'user', content: 'hello', timestamp: 1 });
    manager.appendCustomEntry('note', { a: 1 });
    manager.appendLabelChange(user, 'start');
    manager.appendSessionInfo('Round trip');
    const contextBefore = JSON.stringify(manager.buildSessionContext());
    const leafBefore = manager.getLeafId();
    const warm = manager.appendUsage('cache_warm', 'local', 'tiny', FULL_USAGE, 'warm');
    const warmLeaf = manager.getLeafId();
    const estimate = manager.appendUsage('estimate', 'local', 'tiny', EXTENDED_USAGE, '');
    const explicitNull = manager.appendUsage('null_usage', 'local', 'tiny', null);
    const missing = manager.appendUsage('missing_usage', 'local', 'tiny');
    const usage = {
      explicitNull, missing, nullStored: manager.getEntry(explicitNull.id), missingStored: manager.getEntry(missing.id),
      nullKept: 'usage' in explicitNull && explicitNull.usage === null, missingLeftOut: missing.usage === undefined,
      warm, estimate, warmStored: manager.getEntry(warm.id), estimateStored: manager.getEntry(estimate.id),
      warmParentIsLeaf: warm.parentId === leafBefore, warmLeaf: warmLeaf === warm.id,
      estimateParentIsWarm: estimate.parentId === warm.id, missingLeaf: manager.getLeafId() === missing.id,
      timestamps: [warm, estimate, explicitNull, missing].every((entry) => !Number.isNaN(Date.parse(entry.timestamp))),
      contextSame: JSON.stringify(manager.buildSessionContext()) === contextBefore,
    };
    const file = manager.getSessionFile();
    const reopened = SessionManager.open(file);
    const listed = await SessionManager.list(ctx.cwd, dir);
    const recent = SessionManager.continueRecent(ctx.cwd, dir);
    const memory = SessionManager.inMemory(ctx.cwd);
    memory.appendMessage({ role: 'user', content: 'in memory', timestamp: 2 });
    pi.appendEntry('result', {
      file, sessionId: manager.getSessionId(), reopenedId: reopened.getSessionId(),
      header: reopened.getHeader(), types: reopened.getEntries().map((entry) => entry.type),
      branch: reopened.getBranch().length, label: reopened.getLabel(user), name: reopened.getSessionName(),
      context: reopened.buildSessionContext(), leaf: reopened.getLeafId() === reopened.getEntries().at(-1).id,
      listed: listed.map((info) => ({ id: info.id, name: info.name, dated: info.created instanceof Date })),
      recent: recent.getSessionId(), found: SessionManager.findById(ctx.cwd, manager.getSessionId(), dir) === file,
      memory: memory._sdkDescriptor(), memoryEntries: memory.getEntryCount(),
      usage, badBranch: code(() => manager.branch('missing')),
    });
  }});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_manager_round_trips_an_upstream_jsonl_file() {
    let (_h, data) = run_command(SESSIONS, |_| {}).await;
    let file = data["file"].as_str().expect("session file");
    assert_eq!(data["reopenedId"], data["sessionId"]);
    assert_eq!(data["header"]["type"], "session");
    assert_eq!(data["header"]["version"], 3);
    assert_eq!(
        data["types"],
        json!([
            "model_change",
            "thinking_level_change",
            "message",
            "custom",
            "label",
            "session_info",
            "usage",
            "usage",
            "usage",
            "usage"
        ])
    );
    assert_eq!(data["branch"], 10);
    assert_eq!(data["label"], "start");
    assert_eq!(data["name"], "Round trip");
    assert_eq!(data["leaf"], true);
    assert_eq!(data["context"]["thinkingLevel"], "low");
    assert_eq!(data["context"]["model"], json!({"provider": "local", "modelId": "tiny"}));
    assert_eq!(data["context"]["messages"][0]["content"], "hello");
    assert_eq!(data["listed"], json!([{"id": data["sessionId"], "name": "Round trip", "dated": true}]));
    assert_eq!(data["recent"], data["sessionId"]);
    assert_eq!(data["found"], true);
    assert_eq!(data["memory"]["inMemory"], true);
    assert_eq!(data["memory"]["sessionFile"], Value::Null);
    assert_eq!(data["memoryEntries"], 1);
    let usage = &data["usage"];
    let full = json!({"input": 3, "output": 4, "cacheRead": 5, "cacheWrite": 6, "totalTokens": 18,
        "cost": {"input": 0.5, "output": 0.25, "cacheRead": 0, "cacheWrite": 0, "total": 0.75}});
    let extended = json!({"input": 1.5, "output": 2, "extra": {"nested": [1, "two"], "flag": true}});
    for (key, value) in
        [("type", "usage"), ("kind", "cache_warm"), ("provider", "local"), ("model", "tiny"), ("note", "warm")]
    {
        assert_eq!(usage["warm"][key], value, "{key}");
    }
    assert_eq!(usage["warm"]["usage"], full);
    assert_eq!(usage["estimate"]["kind"], "estimate");
    assert_eq!(usage["estimate"]["usage"], extended, "partial, extra, and fractional usage is kept as given");
    assert!(usage["estimate"].get("note").is_none(), "an empty note is left out");
    assert_eq!(usage["warmStored"], usage["warm"]);
    assert_eq!(usage["estimateStored"], usage["estimate"]);
    assert_eq!(usage["explicitNull"].get("usage"), Some(&Value::Null), "an explicit null usage is stored");
    assert!(usage["missing"].get("usage").is_none(), "an undefined usage is left out");
    assert_eq!(usage["nullStored"], usage["explicitNull"]);
    assert_eq!(usage["missingStored"], usage["missing"]);
    for check in [
        "warmParentIsLeaf",
        "warmLeaf",
        "estimateParentIsWarm",
        "missingLeaf",
        "nullKept",
        "missingLeftOut",
        "timestamps",
        "contextSame",
    ] {
        assert_eq!(usage[check], true, "{check}");
    }
    assert_eq!(data["badBranch"], "Entry missing not found");

    let lines: Vec<Value> =
        std::fs::read_to_string(file).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(lines.len(), 11);
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[3]["message"]["content"], "hello");
    assert_eq!(lines[4]["data"], json!({"a": 1}));
    assert_eq!(lines[7], usage["warm"]);
    assert_eq!(lines[8], usage["estimate"]);
    assert_eq!(lines[9], usage["explicitNull"]);
    assert_eq!(lines[9].get("usage"), Some(&Value::Null));
    assert_eq!(lines[10], usage["missing"]);
    assert!(lines[10].get("usage").is_none());
    let keys: Vec<&str> = lines[7].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["type", "id", "parentId", "timestamp", "kind", "provider", "model", "usage", "note"]);
    let reopened = SessionManager::open(std::path::Path::new(file)).unwrap();
    assert_eq!(reopened.id(), data["sessionId"]);
    assert_eq!(reopened.session_name().as_deref(), Some("Round trip"));
}

#[test]
fn the_typed_append_usage_stores_the_serialized_usage() {
    let mut manager = SessionManager::in_memory(std::path::Path::new("/tmp"));
    let usage = pi_ai::Usage { input: 3, output: 4, total_tokens: 7, ..pi_ai::Usage::default() };
    let id = manager.append_usage("cache_warm", "local", "tiny", &usage, Some("")).unwrap();
    let entry = manager.get_entry(&id).unwrap();
    assert_eq!(entry.raw().get("usage"), Some(&serde_json::to_value(usage).unwrap()));
    assert_eq!(entry.raw().get("kind"), Some(&json!("cache_warm")));
    assert!(entry.raw().get("note").is_none());
    assert_eq!(manager.leaf_id(), Some(id.as_str()));
}

const SETTINGS: &str = r#"
import { getAgentDir, SettingsManager } from '@earendil-works/pi-coding-agent';
const code = (run) => { try { run(); return null; } catch (error) { return error.code ?? error.message; } };
export default function (pi) {
  pi.registerCommand('probe', { handler: async (_args, ctx) => {
    const settings = SettingsManager.create(ctx.cwd, getAgentDir());
    const before = { provider: settings.getDefaultProvider(), theme: settings.getTheme(),
      compaction: settings.getCompactionEnabled(), skills: settings.getSkillPaths() };
    settings.setDefaultModel('written-model');
    settings.setCompactionEnabled(false);
    await settings.flush();
    const memory = SettingsManager.inMemory({ theme: 'light' });
    pi.appendEntry('result', {
      before, global: settings.getGlobalSettings().defaultModel, compaction: settings.getCompactionEnabled(),
      trusted: settings.isProjectTrusted(), errors: settings.drainErrors(), memoryTheme: memory.getTheme(),
      tuiMode: settings.getTuiMode(), fromStorage: code(() => SettingsManager.fromStorage({})),
    });
  }});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_manager_reads_and_writes_the_settings_file() {
    let (h, data) = run_command(SETTINGS, |h| {
        let settings = json!({"defaultProvider": "stored-provider", "theme": "dark", "skills": ["./skills"]});
        std::fs::write(h.agent_dir.join("settings.json"), settings.to_string()).unwrap();
    })
    .await;
    assert_eq!(
        data["before"],
        json!({"provider": "stored-provider", "theme": "dark", "compaction": true, "skills": ["./skills"]})
    );
    assert_eq!(data["global"], "written-model");
    assert_eq!(data["compaction"], false);
    assert_eq!(data["trusted"], true);
    assert_eq!(data["errors"], json!([]));
    assert_eq!(data["memoryTheme"], "light");
    assert_eq!(data["tuiMode"], "fullscreen");
    assert_eq!(data["fromStorage"], Value::Null, "a storage without withLock records a load error");
    let file: Value =
        serde_json::from_str(&std::fs::read_to_string(h.agent_dir.join("settings.json")).unwrap()).unwrap();
    assert_eq!(file["defaultModel"], "written-model");
    assert_eq!(file["defaultProvider"], "stored-provider");
    assert_eq!(file["compaction"], json!({"enabled": false}));
}

const RESOURCES: &str = r#"
import { DefaultResourceLoader, getAgentDir, ModelRegistry, ModelRuntime } from '@earendil-works/pi-coding-agent';
const code = async (run) => { try { await run(); return null; } catch (error) { return error.code ?? error.message; } };
export default function (pi) {
  pi.registerCommand('probe', { handler: async (_args, ctx) => {
    const agentDir = getAgentDir();
    const loader = new DefaultResourceLoader({ cwd: ctx.cwd, agentDir, noSkills: true,
      appendSystemPrompt: ['Extra rule'], systemPromptOverride: (base) => `${base.trim()}!` });
    const beforeReload = loader.getSystemPrompt() ?? null;
    await loader.reload();
    const runtime = await ModelRuntime.create();
    pi.appendEntry('result', {
      beforeReload, systemPrompt: loader.getSystemPrompt(), source: loader.getSystemPromptSource(),
      append: loader.getAppendSystemPrompt(), skills: loader.getSkills(), options: loader._sdkOptions(),
      prompts: await code(() => loader.getPrompts()),
      overrideError: await code(() => new DefaultResourceLoader({ cwd: ctx.cwd, agentDir,
        systemPromptOverride: () => { throw new Error('override failed'); } })._sdkOptions()),
      factories: await code(() => new DefaultResourceLoader({ cwd: ctx.cwd, agentDir, extensionFactories: [() => {}] })),
      hostRuntime: runtime._sdkHostRuntime, model: runtime.getModel('local', 'tiny')?.id,
      models: runtime.getModels('local').map((model) => model.id), auth: runtime.hasConfiguredAuth('local'),
      registry: new ModelRegistry(runtime).find('local', 'other')?.id,
      login: await code(() => runtime.login('local')),
      modelsPath: await code(() => ModelRuntime.create({ modelsPath: '/elsewhere/models.json' })),
    });
  }});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resource_loader_and_model_runtime_serve_child_sessions() {
    let (h, data) = run_command(RESOURCES, |h| {
        std::fs::write(h.agent_dir.join("SYSTEM.md"), "Global system prompt\n").unwrap();
    })
    .await;
    let system_path = h.agent_dir.join("SYSTEM.md").display().to_string();
    assert_eq!(data["beforeReload"], Value::Null);
    assert_eq!(data["systemPrompt"], "Global system prompt!");
    assert_eq!(data["overrideError"], "override failed");
    assert_eq!(data["source"], json!({"path": system_path}));
    assert_eq!(data["append"], json!(["Extra rule"]));
    assert_eq!(data["skills"], json!({"skills": [], "diagnostics": []}));
    let options = &data["options"];
    assert_eq!(options["systemPrompt"], "Global system prompt!");
    assert_eq!(options["appendSystemPrompt"], json!(["Extra rule"]));
    assert_eq!(options["noSkills"], true);
    assert_eq!(options["noExtensions"], false);
    assert!(options["settingsManagerId"].as_u64().is_some(), "{options}");
    assert_eq!(data["prompts"], "ERR_PI_UNSUPPORTED");
    assert_eq!(data["factories"], "ERR_PI_UNSUPPORTED");
    assert_eq!(data["hostRuntime"], true);
    assert_eq!(data["model"], "tiny");
    assert_eq!(data["models"], json!(["tiny", "other"]));
    assert_eq!(data["auth"], true);
    assert_eq!(data["registry"], "other");
    assert_eq!(data["login"], "ERR_PI_UNSUPPORTED");
    assert_eq!(data["modelsPath"], "ERR_PI_UNSUPPORTED");
}
