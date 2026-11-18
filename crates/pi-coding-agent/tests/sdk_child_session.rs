//! SDK `AgentSession` controls of a child session: queueing while streaming, events outside a
//! prompt and their teardown, and model and thinking controls that save only with `persist`.

use crate::extensions::harness;

use harness::{Harness, Setup, harness};
use pi_ai::providers::faux::FauxResponse;
use serde_json::{Value, json};

const PRELUDE: &str = r#"
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { createAgentSession, SessionManager, SettingsManager } from '@earendil-works/pi-coding-agent';
const until = async (check) => {
  for (let i = 0; i < 300 && !check(); i++) await new Promise((resolve) => setTimeout(resolve, 10));
  return check();
};
const child = async (ctx, settingsManager) => (await createAgentSession({
  model: ctx.model, thinkingLevel: 'low', tools: [], sessionManager: SessionManager.inMemory(ctx.cwd),
  settingsManager: settingsManager ?? SettingsManager.inMemory({ retry: { enabled: false }, compaction: { enabled: false } }),
})).session;
"#;

async fn run(command: &str, body: &str, responses: Vec<FauxResponse>) -> (Harness, Value) {
    let source = format!(
        "{PRELUDE}\nexport default pi => pi.registerCommand('{command}', {{ handler: async (_args, ctx) => {{\n{body}\n}}}});\n"
    );
    let h = harness(Setup { files: vec![("child.ts", source)], load: vec!["child.ts"], responses, ..Setup::default() })
        .await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    tokio::time::timeout(std::time::Duration::from_secs(30), h.session.prompt(&format!("/{command}"), Vec::new()))
        .await
        .expect("the command finishes")
        .unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let data = h.entries().iter().find(|entry| entry["customType"] == "result").expect("result entry")["data"].clone();
    (h, data)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompts_while_streaming_queue_with_a_streaming_behavior() {
    let body = r#"
  const session = await child(ctx);
  const updates = [];
  let responding = false;
  const off = session.subscribe((event) => {
    if (event.type === 'message_start' && event.message.role === 'assistant') responding = true;
    if (event.type === 'queue_update') updates.push({ steering: event.steering, followUp: event.followUp });
  });
  const running = session.prompt('one');
  // A run takes the queued steering before its first request, so steer after the response starts.
  const streaming = await until(() => session.isStreaming && responding);
  let refused;
  try { await session.prompt('no behavior'); } catch (error) { refused = error.message; }
  await session.prompt('steered', { streamingBehavior: 'steer' });
  const disposition = await session.followUp('later');
  const queued = { count: session.pendingMessageCount, steering: session.getSteeringMessages(),
    followUp: session.getFollowUpMessages(), idle: session.isIdle };
  const cleared = session.clearQueue();
  const after = session.pendingMessageCount;
  await session.abort();
  await running;
  await session.waitForIdle();
  off();
  pi.appendEntry('result', { streaming, refused, disposition, queued, cleared, after, updates, idle: session.isIdle });
  session.dispose();
"#;
    let (h, data) = run("queue", body, vec![FauxResponse::Hang]).await;
    assert_eq!(data["streaming"], true);
    assert_eq!(
        data["refused"],
        "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
    );
    assert_eq!(data["disposition"], "queued");
    let queued = json!({"count": 2, "steering": ["steered"], "followUp": ["later"], "idle": false});
    assert_eq!(data["queued"], queued);
    assert_eq!(data["cleared"], json!({"steering": ["steered"], "followUp": ["later"]}));
    assert_eq!((data["after"].clone(), data["idle"].clone()), (json!(0), json!(true)));
    let updates = data["updates"].as_array().unwrap();
    assert_eq!(updates.first(), Some(&json!({"steering": ["steered"], "followUp": []})));
    assert!(updates.contains(&json!({"steering": ["steered"], "followUp": ["later"]})), "{updates:?}");
    assert_eq!(updates.last(), Some(&json!({"steering": [], "followUp": []})));
    assert_eq!(h.provider.requests().len(), 1, "queued messages were cleared before they ran");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_reach_listeners_outside_a_prompt_until_they_unsubscribe() {
    let body = r#"
  const session = await child(ctx);
  const levels = (list) => (event) => { if (event.type === 'thinking_level_changed') list.push(event.level); };
  const [first, second, third] = [[], [], []];
  const offFirst = session.subscribe(levels(first));
  const offSecond = session.subscribe(levels(second));
  session.setThinkingLevel('high');
  const delivered = await until(() => first.includes('high') && second.includes('high'));
  offFirst();
  session.setThinkingLevel('medium');
  await until(() => second.includes('medium'));
  offSecond();
  session.setThinkingLevel('off');
  await new Promise((resolve) => setTimeout(resolve, 200));
  const offThird = session.subscribe(levels(third));
  session.setThinkingLevel('low');
  const resubscribed = await until(() => third.includes('low'));
  session.dispose();
  offThird();
  await new Promise((resolve) => setTimeout(resolve, 100));
  pi.appendEntry('result', { delivered, resubscribed, first, second, third });
"#;
    let (h, data) = run("events", body, Vec::new()).await;
    assert_eq!((data["delivered"].clone(), data["resubscribed"].clone()), (json!(true), json!(true)));
    assert_eq!(data["first"], json!(["high"]));
    assert_eq!(data["second"], json!(["high", "medium"]));
    assert_eq!(data["third"], json!(["low"]));
    assert!(h.provider.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_and_thinking_controls_save_only_with_persist() {
    let body = r#"
  const agentDir = `${ctx.cwd}/sdk-agent`;
  mkdirSync(agentDir, { recursive: true });
  const file = () => existsSync(`${agentDir}/settings.json`) ? JSON.parse(readFileSync(`${agentDir}/settings.json`, 'utf8')) : {};
  const session = await child(ctx, SettingsManager.create(ctx.cwd, agentDir));
  session.setThinkingLevel('high');
  const transient = { level: session.thinkingLevel, saved: file().defaultThinkingLevel ?? null };
  session.setThinkingLevel('medium', { persist: true });
  await session.settingsManager.flush();
  const persisted = { level: session.thinkingLevel, saved: file().defaultThinkingLevel ?? null };
  await session.setModel(ctx.model);
  const modelTransient = file().defaultModel ?? null;
  await session.setModel(ctx.model, { persist: true });
  await session.settingsManager.flush();
  const modelPersisted = [file().defaultProvider, file().defaultModel];
  const cycled = session.cycleThinkingLevel();
  session.setSteeringMode('all');
  session.setAutoCompactionEnabled(false);
  session.setAutoRetryEnabled(false);
  await session.settingsManager.flush();
  session.setSessionName('child name');
  const state = session.state;
  await session.waitForIdle();
  pi.appendEntry('result', {
    transient, persisted, modelTransient, modelPersisted, provider: ctx.model.provider,
    cycled, cycledLevel: session.thinkingLevel, savedAfterCycle: file().defaultThinkingLevel,
    steeringMode: session.steeringMode, savedSteeringMode: file().steeringMode,
    autoCompaction: session.autoCompactionEnabled, autoRetry: session.autoRetryEnabled,
    retry: [session.retryAttempt, session.isRetrying], bash: [session.isBashRunning, session.hasPendingBashMessages],
    name: session.sessionName, supports: session.supportsThinking(), levels: session.getAvailableThinkingLevels(),
    activeTools: session.getActiveToolNames(), idle: [session.isIdle, session.isCompacting],
    state: { keys: Object.keys(state).sort(), model: state.model.id, pendingIsSet: state.pendingToolCalls instanceof Set,
      streaming: state.isStreaming, level: state.thinkingLevel },
  });
  session.dispose();
"#;
    let (h, data) = run("controls", body, Vec::new()).await;
    assert_eq!(data["transient"], json!({"level": "high", "saved": null}));
    assert_eq!(data["persisted"], json!({"level": "medium", "saved": "medium"}));
    assert_eq!(data["modelTransient"], Value::Null);
    assert_eq!(data["modelPersisted"], json!([data["provider"], "tiny"]));
    assert_eq!(data["cycled"], data["cycledLevel"]);
    assert_eq!(data["savedAfterCycle"], "medium", "cycling without persist keeps the saved default");
    assert_eq!((data["steeringMode"].clone(), data["savedSteeringMode"].clone()), (json!("all"), json!("all")));
    assert_eq!((data["autoCompaction"].clone(), data["autoRetry"].clone()), (json!(false), json!(false)));
    assert_eq!((data["retry"].clone(), data["bash"].clone()), (json!([0, false]), json!([false, false])));
    assert_eq!((data["name"].clone(), data["supports"].clone()), (json!("child name"), json!(true)));
    assert!(data["levels"].as_array().unwrap().contains(&json!("high")));
    assert_eq!((data["activeTools"].clone(), data["idle"].clone()), (json!([]), json!([true, false])));
    let keys = ["errorMessage", "isStreaming", "messages", "model", "pendingToolCalls", "streamingMessage"];
    let state = &data["state"];
    for key in keys.into_iter().chain(["systemPrompt", "thinkingLevel", "tools"]) {
        assert!(state["keys"].as_array().unwrap().contains(&json!(key)), "{key}: {state}");
    }
    assert_eq!(state["model"], "tiny");
    assert_eq!((state["pendingIsSet"].clone(), state["streaming"].clone()), (json!(true), json!(false)));
    assert_eq!(state["level"], data["cycledLevel"]);
    assert!(h.provider.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_members_stay_unsupported() {
    let body = r#"
  const session = await child(ctx);
  const members = ['agent', 'getToolDefinition', 'navigateTree', 'executeBash', 'sendUserMessage',
    'sendCustomMessage', 'cacheWarmingStatus', 'reload', 'extensionRunner'];
  const errors = members.map((member) => {
    try { session[member]; return [member, null]; } catch (error) { return [member, error.code]; }
  });
  session.dispose();
  pi.appendEntry('result', { errors });
"#;
    let (_h, data) = run("deferred", body, Vec::new()).await;
    assert_eq!(data["errors"].as_array().unwrap().len(), 9);
    for pair in data["errors"].as_array().unwrap() {
        assert_eq!(pair[1], "ERR_PI_UNSUPPORTED", "{pair}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_models_cycle_with_the_scoped_thinking_level() {
    let body = r#"
  const [tiny, other] = [ctx.modelRegistry.find('local', 'tiny'), ctx.modelRegistry.find('local', 'other')];
  const session = await child(ctx);
  session.setScopedModels([{ model: tiny, thinkingLevel: 'high' }, { model: other }]);
  session.setThinkingLevel('high');
  const first = await session.cycleModel();
  const afterFirst = [session.model.id, session.thinkingLevel];
  const second = await session.cycleModel();
  const afterSecond = [session.model.id, session.thinkingLevel];
  const scoped = session.scopedModels.map((entry) => [entry.model.provider, entry.model.id, entry.thinkingLevel ?? null]);
  session.dispose();
  pi.appendEntry('result', { afterFirst, afterSecond, scoped,
    first: [first.model.id, first.thinkingLevel, first.isScoped], second: [second.model.id, second.isScoped] });
"#;
    let (h, data) = run("scoped-cycle", body, Vec::new()).await;
    assert_eq!(data["afterFirst"], json!(["other", "off"]));
    assert_eq!(data["afterSecond"], json!(["tiny", "high"]));
    assert_eq!(data["first"], json!(["other", "off", true]));
    assert_eq!(data["second"], json!(["tiny", true]));
    assert_eq!(data["scoped"], json!([["local", "tiny", "high"], ["local", "other", null]]));
    assert!(h.provider.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_models_option_sets_the_scope_without_choosing_the_model() {
    let body = r#"
  const other = ctx.modelRegistry.find('local', 'other');
  const { session } = await createAgentSession({
    thinkingLevel: 'low', tools: [], scopedModels: [{ model: other, thinkingLevel: 'high' }],
    sessionManager: SessionManager.inMemory(ctx.cwd),
    settingsManager: SettingsManager.inMemory({ defaultProvider: 'local', defaultModel: 'tiny',
      retry: { enabled: false }, compaction: { enabled: false } }),
  });
  const ids = () => session.scopedModels.map((entry) => [entry.model.id, entry.thinkingLevel ?? null]);
  const initial = { model: session.model.id, level: session.thinkingLevel, scoped: ids() };
  let invalid = null;
  try { session.setScopedModels([{ model: other, thinkingLevel: 'loud' }]); } catch (error) { invalid = error.message; }
  const afterInvalid = ids();
  session.setScopedModels([]);
  const cleared = ids();
  const cycled = await session.cycleModel();
  session.dispose();
  pi.appendEntry('result', { initial, invalid, afterInvalid, cleared, cycledScoped: cycled?.isScoped ?? null });
"#;
    let (h, data) = run("scoped-option", body, Vec::new()).await;
    assert_eq!(data["initial"], json!({"model": "tiny", "level": "low", "scoped": [["other", "high"]]}));
    assert!(data["invalid"].is_string(), "{data}");
    assert_eq!(data["afterInvalid"], json!([["other", "high"]]), "a rejected scope leaves the scope unchanged");
    assert_eq!(data["cleared"], json!([]));
    assert_eq!(data["cycledScoped"], false, "an empty scope cycles through the available models");
    assert!(h.provider.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_set_model_adds_to_a_non_empty_scope() {
    let body = r#"
  const agentDir = `${ctx.cwd}/sdk-agent`;
  mkdirSync(agentDir, { recursive: true });
  writeFileSync(`${agentDir}/settings.json`, JSON.stringify({ enabledModels: ['local/tiny'],
    retry: { enabled: false }, compaction: { enabled: false } }));
  const file = () => JSON.parse(readFileSync(`${agentDir}/settings.json`, 'utf8'));
  const [tiny, other] = [ctx.modelRegistry.find('local', 'tiny'), ctx.modelRegistry.find('local', 'other')];
  const settingsManager = SettingsManager.create(ctx.cwd, agentDir);
  const { session } = await createAgentSession({ model: tiny, tools: [], scopedModels: [{ model: tiny }],
    sessionManager: SessionManager.inMemory(ctx.cwd), settingsManager });
  const ids = () => session.scopedModels.map((entry) => entry.model.id);
  await session.setModel(other);
  const transient = ids();
  await session.setModel(other, { persist: true });
  await settingsManager.flush();
  const persisted = { scoped: ids(), enabled: file().enabledModels, model: file().defaultModel };
  await session.setModel(tiny, { persist: true });
  await settingsManager.flush();
  const known = { scoped: ids(), enabled: file().enabledModels };
  session.setScopedModels([]);
  await session.setModel(other, { persist: true });
  await settingsManager.flush();
  const empty = { scoped: ids(), enabled: file().enabledModels };
  session.dispose();
  pi.appendEntry('result', { transient, persisted, known, empty });
"#;
    let (h, data) = run("scoped-persist", body, Vec::new()).await;
    assert_eq!(data["transient"], json!(["tiny"]), "setModel without persist keeps the scope");
    let both = json!({"scoped": ["tiny", "other"], "enabled": ["local/tiny", "local/other"]});
    assert_eq!(data["persisted"], json!({"scoped": both["scoped"], "enabled": both["enabled"], "model": "other"}));
    assert_eq!(data["known"], both, "a model already in the scope is not added again");
    assert_eq!(data["empty"], json!({"scoped": [], "enabled": both["enabled"]}), "an empty scope stays empty");
    assert!(h.provider.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_storage_receives_only_explicitly_persisted_controls() {
    let body = r#"
  let text = '{"keep":1}';
  const writes = [];
  const settings = SettingsManager.fromStorage({ withLock(scope, fn) {
    const next = fn(scope === 'global' ? text : undefined);
    if (next !== undefined) {
      if (scope !== 'global') throw new Error('unexpected project write');
      text = next;
      writes.push(JSON.parse(next));
    }
  }});
  const session = await child(ctx, settings);
  session.setThinkingLevel('high');
  await session.setModel(ctx.model);
  await settings.flush();
  const transient = { saved: JSON.parse(text), writes: writes.length };
  session.setThinkingLevel('medium', { persist: true });
  await settings.flush();
  const thinking = JSON.parse(text);
  await session.setModel(ctx.model, { persist: true });
  await settings.flush();
  const model = JSON.parse(text);
  session.cycleThinkingLevel();
  await settings.flush();
  const afterCycle = JSON.parse(text);
  session.dispose();
  pi.appendEntry('result', { transient, thinking, model, afterCycle, writes, provider: ctx.model.provider,
    errors: settings.drainErrors().length });
"#;
    let (h, data) = run("custom-controls", body, Vec::new()).await;
    assert_eq!(data["transient"], json!({"saved": {"keep": 1}, "writes": 0}));
    assert_eq!(data["thinking"], json!({"keep": 1, "defaultThinkingLevel": "medium"}));
    let model = json!({"keep": 1, "defaultThinkingLevel": "medium",
        "defaultProvider": data["provider"], "defaultModel": "tiny"});
    assert_eq!(data["model"], model);
    assert_eq!(data["afterCycle"], model, "cycling without persist leaves custom storage unchanged");
    assert_eq!(data["writes"], json!([data["thinking"], model]));
    assert_eq!(data["errors"], 0);
    assert!(h.provider.requests().is_empty());
}
