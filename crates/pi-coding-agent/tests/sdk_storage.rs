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
      usage: code(() => manager.appendUsage()), badBranch: code(() => manager.branch('missing')),
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
        json!(["model_change", "thinking_level_change", "message", "custom", "label", "session_info"])
    );
    assert_eq!(data["branch"], 6);
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
    assert_eq!(data["usage"], "ERR_PI_UNSUPPORTED");
    assert_eq!(data["badBranch"], "Entry missing not found");

    let lines: Vec<Value> =
        std::fs::read_to_string(file).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(lines.len(), 7);
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[3]["message"]["content"], "hello");
    assert_eq!(lines[4]["data"], json!({"a": 1}));
    let reopened = SessionManager::open(std::path::Path::new(file)).unwrap();
    assert_eq!(reopened.id(), data["sessionId"]);
    assert_eq!(reopened.session_name().as_deref(), Some("Round trip"));
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
    const memory = SettingsManager.inMemory({ theme: 'light' });
    pi.appendEntry('result', {
      before, global: settings.getGlobalSettings().defaultModel, compaction: settings.getCompactionEnabled(),
      trusted: settings.isProjectTrusted(), errors: settings.drainErrors(), memoryTheme: memory.getTheme(),
      unsupported: code(() => settings.getTuiMode()), fromStorage: code(() => SettingsManager.fromStorage({})),
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
    assert_eq!(data["unsupported"], "ERR_PI_UNSUPPORTED");
    assert_eq!(data["fromStorage"], "ERR_PI_UNSUPPORTED");
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
