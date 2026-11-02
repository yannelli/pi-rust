//! A child session with both a `DefaultResourceLoader` and a created `ModelRuntime`: the providers
//! and virtual models of its file and factory extensions bind to that runtime, not the host's.

use super::*;

const COMPOSED: &str = r#"
    const ids = (registry) => [['file-prov', 'f1'], ['factory-prov', 'g1'], ['vroute', 'auto'], ['beta', 'b1']]
      .map(([provider, id]) => registry.find(provider, id)?.id ?? null);
    const model = (id) => ({ id, name: id, reasoning: false, input: ['text'],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 1000, maxTokens: 100 });
    const factory = (pi) => {
      pi.registerProvider('factory-prov', { baseUrl: 'http://127.0.0.1:9', api: 'openai-completions',
        apiKey: 'factory-key', models: [model('g1')] });
      pi.registerVirtualModel({ provider: 'vroute', id: 'auto', name: 'Auto', contextWindow: 1000, maxTokens: 100,
        route: () => ({ model: { provider: 'factory-prov', id: 'g1' } }) });
      pi.registerCommand('factory-probe', { handler: async (_args, commandCtx) => {
        result.factoryCtx = ids(commandCtx.modelRegistry);
      } });
    };
    const runtimeDir = join(root, 'runtime-b');
    const b = await ModelRuntime.create({ modelsPath: join(runtimeDir, 'models.json'),
      authPath: join(runtimeDir, 'auth.json') });
    const loader = new DefaultResourceLoader({ cwd: ctx.cwd, agentDir,
      additionalExtensionPaths: [join(root, 'ext', 'file-provider.ts')], extensionFactories: [factory] });
    await loader.reload();
    const host = () => [ids(ctx.modelRegistry), ids(new ModelRegistry({ _sdkRuntimeId: 0 }))];
    const options = (modelRuntime) => ({ resourceLoader: loader, modelRuntime, model: b.getModel('beta', 'b1'),
      sessionManager: SessionManager.inMemory(ctx.cwd),
      settingsManager: SettingsManager.inMemory({ retry: { enabled: false }, compaction: { enabled: false } }) });
    result.unknown = await fail(() => createAgentSession(options({ _sdkRuntimeId: 987654 })));
    result.beforeBind = ids(b);
    const { session } = await createAgentSession(options(b));
    result.sameRuntime = session.modelRuntime === b;
    result.model = session.model?.id ?? null;
    result.child = ids(b);
    result.host = host();
    await session.prompt('/file-probe');
    await session.prompt('/factory-probe');
    result.fileProbe = session.sessionManager.getEntries()
      .find((entry) => entry.type === 'custom' && entry.customType === 'file-probe')?.data ?? null;
    session.dispose();
    result.afterDispose = ids(b);
    result.hostAfterDispose = host();
"#;

const FILE_PROVIDER: &str = r#"
import { ModelRegistry } from '@earendil-works/pi-coding-agent';
const ids = (registry) => [['file-prov', 'f1'], ['factory-prov', 'g1'], ['vroute', 'auto'], ['beta', 'b1']]
  .map(([provider, id]) => registry.find(provider, id)?.id ?? null);
export default (pi) => {
  pi.registerProvider('file-prov', { baseUrl: 'http://127.0.0.1:9', api: 'openai-completions', apiKey: 'file-key',
    models: [{ id: 'f1', name: 'F1', reasoning: false, input: ['text'],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 1000, maxTokens: 100 }] });
  pi.registerCommand('file-probe', { handler: async (_args, ctx) => {
    pi.appendEntry('file-probe', { ctx: ids(ctx.modelRegistry), runtime0: ids(new ModelRegistry({ _sdkRuntimeId: 0 })) });
  } });
};
"#;

fn runtime_b(root: &Path) {
    let models = json!({"providers": {"beta": {"baseUrl": "http://127.0.0.1:9", "api": "openai-completions",
        "models": [{"id": "b1", "contextWindow": 1000, "maxTokens": 100}]}}});
    write(&root.join("runtime-b/models.json"), &models.to_string());
    write(&root.join("runtime-b/auth.json"), &json!({"beta": {"type": "api_key", "key": "beta-key"}}).to_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loader_extensions_bind_their_models_to_the_child_runtime() {
    let (_h, _root, data) = run(COMPOSED, |_h, root| {
        write(&root.join("ext/file-provider.ts"), FILE_PROVIDER);
        runtime_b(root);
    })
    .await;
    let all = json!(["f1", "g1", "auto", "b1"]);
    let none = json!([null, null, null, null]);
    assert_eq!(data["unknown"], "Unknown ModelRuntime: 987654", "{data}");
    assert_eq!(data["beforeBind"], json!([null, null, null, "b1"]), "reload() binds no models");
    assert_eq!((&data["sameRuntime"], &data["model"]), (&json!(true), &json!("b1")));
    assert_eq!(data["child"], all, "the rejected runtime left the reload usable, and both kinds bound to B");
    assert_eq!(data["host"], json!([none, none]), "the host runtime has none of them");
    assert_eq!(data["fileProbe"], json!({"ctx": all, "runtime0": all}), "{data}");
    assert_eq!(data["factoryCtx"], all);
    assert_eq!(data["afterDispose"], json!([null, null, null, "b1"]), "dispose removes only the loader's models");
    assert_eq!(data["hostAfterDispose"], json!([none, none]));
}

const COLLISION: &str = r#"
    const message = (model, text) => ({ role: 'assistant', content: [{ type: 'text', text }], api: model.api,
      provider: model.provider, model: model.id, stopReason: 'stop', timestamp: Date.now(),
      usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0,
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } } });
    const provider = (who, id) => ({ baseUrl: 'http://127.0.0.1:9', api: `dup-${who}-api`, apiKey: `${who}-key`,
      models: [{ id, name: id, reasoning: false, input: ['text'],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 1000, maxTokens: 100 }],
      async *streamSimple(model, _context, options) {
        const reply = message(model, `${who}:${model.id}:${options.apiKey}`);
        yield { type: 'start', partial: reply };
        yield { type: 'done', reason: 'stop', message: reply };
      } });
    pi.registerProvider('dup', provider('host', 'h1'));
    const b = await ModelRuntime.create({ modelsPath: join(root, 'runtime-b', 'models.json'),
      authPath: join(root, 'runtime-b', 'auth.json') });
    const loader = new DefaultResourceLoader({ cwd: ctx.cwd, agentDir,
      extensionFactories: [(factoryPi) => factoryPi.registerProvider('dup', provider('factory', 'c1'))] });
    await loader.reload();
    const open = (options) => createAgentSession({ ...options, sessionManager: SessionManager.inMemory(ctx.cwd),
      settingsManager: SettingsManager.inMemory({ retry: { enabled: false }, compaction: { enabled: false } }) });
    const replies = (session) => session.sessionManager.getEntries()
      .filter((entry) => entry.type === 'message' && entry.message.role === 'assistant')
      .map((entry) => entry.message.content.map((part) => part.text ?? '').join('') || entry.message.errorMessage);
    const models = () => ({ b: [b.getModel('dup', 'h1')?.id ?? null, b.getModel('dup', 'c1')?.id ?? null],
      host: [ctx.modelRegistry.find('dup', 'h1')?.id ?? null, ctx.modelRegistry.find('dup', 'c1')?.id ?? null] });
    const { session: child } = await open({ resourceLoader: loader, modelRuntime: b, model: b.getModel('beta', 'b1') });
    result.created = [b.getModel('dup', 'c1')?.id ?? null, child.model?.id ?? null];
    await child.setModel(b.getModel('dup', 'c1'));
    result.selected = child.model?.id ?? null;
    const { session: hostChild } = await open({ model: ctx.modelRegistry.find('dup', 'h1') });
    await child.prompt('one');
    await hostChild.prompt('two');
    result.bound = models();
    result.child = replies(child);
    child.dispose();
    await hostChild.prompt('three');
    result.disposed = models();
    result.host = replies(hostChild);
    hostChild.dispose();
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_named_host_and_factory_providers_stay_apart() {
    let (_h, _root, data) = run(COLLISION, |_h, root| runtime_b(root)).await;
    assert_eq!(data["created"], json!(["c1", "b1"]), "create binds the factory provider to B");
    assert_eq!(data["selected"], "c1", "{data}");
    assert_eq!(data["bound"], json!({"b": [null, "c1"], "host": ["h1", null]}), "{data}");
    assert_eq!(data["child"], json!(["factory:c1:factory-key"]), "the child streams through the factory callback");
    assert_eq!(data["disposed"], json!({"b": [null, null], "host": ["h1", null]}), "{data}");
    assert_eq!(data["host"], json!(["host:h1:host-key", "host:h1:host-key"]), "the host callback survives dispose");
}
