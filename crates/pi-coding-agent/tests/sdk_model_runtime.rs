//! SDK `ModelRuntime.create` for extensions: separate native runtimes with their own `models.json`
//! and `auth.json`, runtime API keys, typed reads, offline refresh, child sessions, provider
//! objects, and login.

use crate::extensions::harness;

use std::path::Path;

use harness::{Setup, harness};
use serde_json::{Value, json};

const PROBE: &str = r#"
import { createAgentSession, ModelRegistry, ModelRuntime } from '@earendil-works/pi-coding-agent';
const code = async (run) => { try { await run(); return null; } catch (error) { return error.code ?? error.message; } };
const ids = (models) => models.map((model) => model.id);
const providers = (list) => list.map((credential) => credential.providerId).sort();

export default async function (pi) {
  const a = await ModelRuntime.create({ modelsPath: __A_MODELS__, authPath: __A_AUTH__ });
  const b = await ModelRuntime.create({ modelsPath: __B_MODELS__, authPath: __B_AUTH__, refreshOnCreate: false });
  await a.setRuntimeApiKey('alpha', 'runtime-key');
  const load = {
    distinct: a._sdkRuntimeId !== b._sdkRuntimeId && a._sdkRuntimeId > 0,
    aModels: ids(a.getModels('alpha')), bModels: ids(b.getModels('alpha')), hostModels: ids(a.getModels('local')),
    aAuth: a.hasConfiguredAuth('alpha'), bAuth: b.hasConfiguredAuth('alpha'),
    aStatus: a.getProviderAuthStatus('alpha'), bStatus: b.getProviderAuthStatus('alpha'),
    aKey: await a.getApiKeyForProvider('alpha') ?? null, bKey: await b.getApiKeyForProvider('alpha') ?? null,
    aListed: providers(await a.listCredentials()), bListed: providers(await b.listCredentials()),
    aAvailable: ids(await a.getAvailable('alpha')), snapshot: ids(a.getAvailableSnapshot().filter((model) => model.provider === 'alpha')),
    registry: new ModelRegistry(b).find('alpha', 'chat-a')?.id ?? null,
    providerIds: a.getProviders().map((provider) => provider.id),
    chatOnly: a.getModels('openrouter').every((model) => (model.type ?? 'chat') === 'chat'),
    images: a.getModelsOfType('image', 'openrouter').length > 0,
    allHasImages: a.getAllModels('openrouter').some((model) => model.type === 'image'),
  };
  await a.removeRuntimeApiKey('alpha');
  load.removed = { auth: a.hasConfiguredAuth('alpha'), listed: providers(await a.listCredentials()),
    key: await a.getApiKeyForProvider('alpha') ?? null };
  pi.registerCommand('probe', { handler: async (_args, ctx) => {
    const offline = await b.refresh({ allowNetwork: false, providers: ['alpha'] });
    const controller = new AbortController();
    controller.abort();
    const aborted = await b.refresh({ allowNetwork: false, signal: controller.signal });
    const { session } = await createAgentSession({ cwd: ctx.cwd, modelRuntime: b, noTools: true });
    const child = { model: session.model?.id ?? null, sameRuntime: session.modelRuntime === b };
    session.dispose();
    pi.appendEntry('result', { ...load, child,
      offline: { aborted: offline.aborted, errors: [...offline.errors.keys()] }, aborted: aborted.aborted,
      hostAlpha: ctx.modelRegistry.find('alpha', 'chat-a') ?? null,
      hostAlphaAuth: ctx.modelRegistry.hasConfiguredAuth({ provider: 'alpha' }),
      unsupported: [
        await code(() => ModelRuntime.create({ credentials: {} })),
        await code(() => ModelRuntime.create({ modelsStore: {} })),
        await code(() => ModelRuntime.create({ modelsPath: null })),
        await code(() => a.registerVirtualModel({ provider: 'alpha', id: 'v' })),
        await code(() => a.registerProvider('x', {})),
        await code(() => a.stream(a.getModel('alpha', 'chat-a'), { messages: [] })),
      ],
    });
  }});
}
"#;

fn write_runtime_dir(dir: &Path, auth: Option<Value>) {
    std::fs::create_dir_all(dir).unwrap();
    let models = json!({"providers": {"alpha": {"baseUrl": "http://127.0.0.1:9", "api": "openai-completions",
        "models": [{"id": "chat-a", "contextWindow": 1000, "maxTokens": 100}]}}});
    std::fs::write(dir.join("models.json"), models.to_string()).unwrap();
    if let Some(auth) = auth {
        std::fs::write(dir.join("auth.json"), auth.to_string()).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn created_runtimes_are_isolated_native_runtimes() {
    let root = tempfile::tempdir().unwrap();
    let (a, b) = (root.path().join("a"), root.path().join("b"));
    write_runtime_dir(&a, None);
    write_runtime_dir(&b, Some(json!({"alpha": {"type": "api_key", "key": "stored-b"}})));
    let literal = |dir: &Path, file: &str| serde_json::to_string(&dir.join(file).display().to_string()).unwrap();
    let source = PROBE
        .replace("__A_MODELS__", &literal(&a, "models.json"))
        .replace("__A_AUTH__", &literal(&a, "auth.json"))
        .replace("__B_MODELS__", &literal(&b, "models.json"))
        .replace("__B_AUTH__", &literal(&b, "auth.json"));
    let h = harness(Setup { files: vec![("probe.ts", source)], load: vec!["probe.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    tokio::time::timeout(std::time::Duration::from_secs(20), h.session.prompt("/probe", Vec::new()))
        .await
        .unwrap()
        .unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entries = h.entries();
    let data = &entries.iter().find(|entry| entry["customType"] == "result").expect("result entry")["data"];

    assert_eq!(data["distinct"], true, "{data}");
    assert_eq!(data["aModels"], json!(["chat-a"]));
    assert_eq!(data["bModels"], json!(["chat-a"]));
    assert_eq!(data["hostModels"], json!([]), "a created runtime does not read the harness models.json");
    assert_eq!((&data["aAuth"], &data["bAuth"]), (&json!(true), &json!(true)));
    assert_eq!(data["aStatus"], json!({"configured": true, "source": "runtime"}));
    assert_eq!(data["bStatus"], json!({"configured": true}));
    assert_eq!((&data["aKey"], &data["bKey"]), (&json!("runtime-key"), &json!("stored-b")));
    assert_eq!((&data["aListed"], &data["bListed"]), (&json!(["alpha"]), &json!(["alpha"])));
    assert_eq!((&data["aAvailable"], &data["snapshot"]), (&json!(["chat-a"]), &json!(["chat-a"])));
    assert_eq!(data["registry"], "chat-a");
    assert!(data["providerIds"].as_array().unwrap().contains(&json!("alpha")), "{data}");
    assert_eq!((&data["chatOnly"], &data["images"], &data["allHasImages"]), (&json!(true), &json!(true), &json!(true)));
    assert_eq!(data["removed"], json!({"auth": false, "listed": [], "key": null}));
    assert!(!a.join("auth.json").exists(), "runtime keys are never written to auth.json");

    assert_eq!(data["offline"], json!({"aborted": false, "errors": []}));
    assert_eq!(data["aborted"], true);
    assert_eq!(data["child"], json!({"model": "chat-a", "sameRuntime": true}), "the child picks from runtime b");
    assert_eq!((&data["hostAlpha"], &data["hostAlphaAuth"]), (&Value::Null, &json!(false)));
    assert_eq!(data["unsupported"], json!(vec!["ERR_PI_UNSUPPORTED"; 6]));
}

const NATIVE_PROBE: &str = r#"
import { ModelRegistry, ModelRuntime } from '@earendil-works/pi-coding-agent';
import { readFileSync } from 'node:fs';

export default function (pi) {
  pi.registerCommand('probe', { handler: async (_args, ctx) => {
    const runtime = await ModelRuntime.create({ modelsPath: __MODELS__, authPath: __AUTH__ });
    const provider = {
      id: 'sdk-native', name: 'SDK Native',
      auth: { apiKey: {
        name: 'SDK key',
        async login(interaction) {
          const key = await interaction.prompt({ type: 'secret', message: 'Key?' });
          return { type: 'api_key', key };
        },
        resolve: async ({ credential }) => (credential?.key ? { auth: { apiKey: credential.key } } : undefined),
      } },
      getModels: () => [{ id: 'm', name: 'M', api: 'sdk-api', provider: 'sdk-native', baseUrl: 'http://127.0.0.1:9',
        reasoning: false, input: ['text'], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: 1000, maxTokens: 100 }],
      stream() { throw new Error('unused'); },
      streamSimple() { throw new Error('unused'); },
    };
    runtime.registerNativeProvider(provider);
    const registry = new ModelRegistry(runtime);
    const before = {
      models: runtime.getModels('sdk-native').map((model) => model.id),
      same: runtime.getRegisteredNativeProvider('sdk-native') === provider,
      registry: registry.getRegisteredNativeProvider('sdk-native') === provider,
      effective: runtime.getProvider('sdk-native') === provider,
      ids: runtime.getRegisteredProviderIds(),
      config: runtime.getRegisteredProviderConfig('sdk-native') ?? null,
    };
    const prompts = [];
    const interaction = { prompt: async (prompt) => { prompts.push(prompt); return 'sdk-key'; }, notify: () => {} };
    const credential = await runtime.login('sdk-native', 'api_key', interaction);
    const key = (await runtime.getApiKeyForProvider('sdk-native')) ?? null;
    const authFile = JSON.parse(readFileSync(__AUTH__, 'utf8'));
    const oauth = await runtime.login('sdk-native', 'oauth', interaction).catch((error) => error.message);
    await runtime.logout('sdk-native');
    const afterLogout = (await runtime.getApiKeyForProvider('sdk-native')) ?? null;
    runtime.unregisterProvider('sdk-native');
    pi.appendEntry('native', { before, prompts, credential, key, authFile, oauth, afterLogout,
      after: { models: runtime.getModels('sdk-native').length, native: runtime.getRegisteredNativeProvider('sdk-native') ?? null },
      host: ctx.modelRegistry.find('sdk-native', 'm') ?? null });
  } });
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn created_runtimes_register_provider_objects_and_log_in() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("runtime");
    write_runtime_dir(&dir, None);
    let literal = |file: &str| serde_json::to_string(&dir.join(file).display().to_string()).unwrap();
    let source = NATIVE_PROBE.replace("__MODELS__", &literal("models.json")).replace("__AUTH__", &literal("auth.json"));
    let h = harness(Setup { files: vec![("probe.ts", source)], load: vec!["probe.ts"], ..Setup::default() }).await;
    tokio::time::timeout(std::time::Duration::from_secs(20), h.session.prompt("/probe", Vec::new()))
        .await
        .unwrap()
        .unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entries = h.entries();
    let data = &entries.iter().find(|entry| entry["customType"] == "native").expect("native entry")["data"];
    let before = json!({"models": ["m"], "same": true, "registry": true, "effective": true, "ids": ["sdk-native"],
        "config": null});
    assert_eq!(data["before"], before);
    assert_eq!(data["prompts"], json!([{"type": "secret", "message": "Key?"}]));
    assert_eq!(data["credential"], json!({"type": "api_key", "key": "sdk-key"}));
    assert_eq!(data["key"], "sdk-key");
    assert_eq!(data["oauth"], "SDK Native does not support oauth login");
    assert_eq!(data["afterLogout"], Value::Null);
    assert_eq!(data["after"], json!({"models": 0, "native": null}));
    assert_eq!(data["host"], Value::Null, "the host runtime does not see the created runtime's providers");
    assert_eq!(data["authFile"], json!({"sdk-native": {"type": "api_key", "key": "sdk-key"}}));
    let auth: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("auth.json")).unwrap()).unwrap();
    assert!(auth.get("sdk-native").is_none(), "logout removed the stored key: {auth}");
}
