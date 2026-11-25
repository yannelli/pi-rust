//! `createExtensionRuntime`, `readStoredCredential`, and `resolveCliModel`, and a child session as
//! pi-goal-x builds it: a custom `ResourceLoader` with a `createExtensionRuntime` runtime, a custom
//! tool, and the runtime of `ctx.modelRegistry`.

use crate::extensions::harness;

use harness::{Setup, harness};
use pi_ai::providers::faux::FauxResponse;
use serde_json::{Value, json};

const SOURCE: &str = r#"
import { mkdirSync, writeFileSync } from 'node:fs';
import { createAgentSession, createExtensionRuntime, defineTool, getAgentDir, readStoredCredential, resolveCliModel,
  SessionManager, SettingsManager } from '@earendil-works/pi-coding-agent';
const capture = (run) => { try { return run(); } catch (error) { return `threw: ${error.message}`; } };
export default pi => pi.registerCommand('exports', { handler: async (_args, ctx) => {
  const runtime = createExtensionRuntime();
  runtime.registerProvider('later', { api: 'x' }, 'ext.ts');
  runtime.registerVirtualModel({ provider: 'later', id: 'v' });
  runtime.unregisterVirtualModel('later', 'v');
  const before = { pending: runtime.pendingProviderRegistrations, virtual: runtime.pendingVirtualModelRegistrations,
    flags: runtime.flagValues instanceof Map, servers: runtime.mcpServers.list(), active: capture(() => runtime.getActiveTools()) };
  runtime.invalidate();
  const stale = capture(() => runtime.assertActive());

  const dir = `${ctx.cwd}/auth-dir`;
  mkdirSync(dir, { recursive: true });
  writeFileSync(`${dir}/auth.json`, '\uFEFF' + JSON.stringify({ litellm: { type: 'oauth', baseUrl: 'https://llm.example' } }));
  writeFileSync(`${getAgentDir()}/auth.json`, JSON.stringify({ other: { type: 'api_key', key: 'k' } }));
  const credentials = { stored: readStoredCredential('litellm', `${dir}/auth.json`), missing: readStoredCredential('none', `${dir}/auth.json`),
    noFile: readStoredCredential('litellm', `${dir}/nothing.json`), fallback: readStoredCredential('other') };

  const submitted = [];
  const submit = defineTool({
    name: 'submit_advice', label: 'Submit', description: 'Submit advice', parameters: { type: 'object', properties: { advice: { type: 'string' } } },
    async execute(_id, params) { submitted.push(params.advice); return { content: [{ type: 'text', text: 'Advice recorded.' }], details: {} }; },
  });
  const loader = {
    getExtensions: () => ({ extensions: [], errors: [], runtime: createExtensionRuntime() }),
    getSkills: () => ({ skills: [], diagnostics: [] }), getPrompts: () => ({ prompts: [], diagnostics: [] }),
    getThemes: () => ({ themes: [], diagnostics: [] }), getAgentsFiles: () => ({ agentsFiles: [] }),
    getSystemPrompt: () => 'You are a read-only adviser.', getSystemPromptSource: () => undefined,
    getAppendSystemPrompt: () => [], getAppendSystemPromptSources: () => [], extendResources: () => {}, reload: async () => {},
  };
  const { session } = await createAgentSession({
    cwd: ctx.cwd, model: ctx.model, modelRuntime: ctx.modelRegistry.runtime, sessionManager: SessionManager.inMemory(ctx.cwd),
    settingsManager: SettingsManager.inMemory({ compaction: { enabled: false } }), resourceLoader: loader,
    tools: ['read', 'grep', 'find', 'ls', 'submit_advice'], customTools: [submit],
  });
  await session.prompt('advise');
  const resolved = (cliModel, extra = {}) => {
    const result = resolveCliModel({ cliModel, modelRuntime: session.modelRuntime, ...extra });
    return { model: result.model && `${result.model.provider}/${result.model.id}`, thinkingLevel: result.thinkingLevel ?? null,
      warning: result.warning ?? null, error: result.error ?? null };
  };
  const models = { exact: resolved('local/tiny'), level: resolved('tiny:high'), custom: resolved('local/custom-id'),
    provider: resolved('tiny', { cliProvider: 'nope' }), none: resolveCliModel({ modelRuntime: session.modelRuntime }),
    foreign: capture(() => resolveCliModel({ cliModel: 'tiny', modelRuntime: {} })) };
  pi.appendEntry('result', { before, stale, credentials, submitted, active: session.getActiveToolNames(), models });
  session.dispose();
}});
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_credential_and_cli_model_exports_work_as_upstream() {
    let advice = json!({"advice": "try the other path"});
    let h = harness(Setup {
        files: vec![("exports.ts", SOURCE.into())],
        load: vec!["exports.ts"],
        responses: vec![FauxResponse::tool_call("call-1", "submit_advice", advice), FauxResponse::text("done")],
        ..Setup::default()
    })
    .await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    tokio::time::timeout(std::time::Duration::from_secs(30), h.session.prompt("/exports", Vec::new()))
        .await
        .expect("the command finishes")
        .unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let data = h.entries().iter().find(|entry| entry["customType"] == "result").expect("result")["data"].clone();
    let before = &data["before"];
    assert_eq!(before["pending"], json!([{"name": "later", "config": {"api": "x"}, "extensionPath": "ext.ts"}]));
    assert_eq!(
        (before["virtual"].clone(), before["flags"].clone(), before["servers"].clone()),
        (json!([]), json!(true), json!([]))
    );
    let not_initialized =
        "threw: Extension runtime not initialized. Action methods cannot be called during extension loading.";
    assert_eq!(before["active"], not_initialized);
    assert!(data["stale"].as_str().unwrap().starts_with("threw: This extension ctx is stale"));
    let credentials = &data["credentials"];
    assert_eq!(credentials["stored"], json!({"type": "oauth", "baseUrl": "https://llm.example"}));
    assert_eq!(credentials.get("missing"), None);
    assert_eq!(credentials.get("noFile"), None);
    assert_eq!(credentials["fallback"], json!({"type": "api_key", "key": "k"}));
    assert_eq!(data["submitted"], json!(["try the other path"]));
    let mut active: Vec<String> = serde_json::from_value(data["active"].clone()).unwrap();
    active.sort();
    assert_eq!(active, ["find", "grep", "ls", "read", "submit_advice"]);
    let request = serde_json::to_string(&h.provider.requests()[0]).unwrap();
    assert!(request.contains("You are a read-only adviser."));
    let models = &data["models"];
    let model = |provider_id: &str, level: Value, warning: Value| json!({"model": provider_id, "thinkingLevel": level, "warning": warning, "error": null});
    assert_eq!(models["exact"], model("local/tiny", Value::Null, Value::Null));
    assert_eq!(models["level"], model("local/tiny", json!("high"), Value::Null));
    let custom = json!("Model \"custom-id\" not found for provider \"local\". Using custom model id.");
    assert_eq!(models["custom"], model("local/custom-id", Value::Null, custom));
    let unknown = "Unknown provider \"nope\". Use --list-models to see available providers/models.";
    assert_eq!(models["provider"], json!({"thinkingLevel": null, "warning": null, "error": unknown}));
    assert_eq!(models["none"], json!({}));
    assert_eq!(models["foreign"], "threw: resolveCliModel.modelRuntime is not supported in pi-rust");
}
