//! Runtime exports of the bundled modules beyond the session SDK: coding-agent utilities, pi-ai
//! helpers, pi-agent-core, and the type-only oauth entry, run from extensions over the faux provider.

#[path = "extensions/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use harness::{Harness, Setup, harness};
use pi_ai::providers::faux::FauxResponse;
use serde_json::{Value, json};

const PNG_4X2: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAQAAAACCAIAAADwyuo0AAAAEElEQVR4nGP4z8AARwzIHABvqgf5gNwAKAAAAABJRU5ErkJggg==";

const UTILITIES: &str = r#"
import * as ca from '@earendil-works/pi-coding-agent';
import { writeFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';

const PNG = '__PNG__';
const code = (fn) => { try { fn(); return 'none'; } catch (error) { return `${error.code}: ${error.message}`; } };

export default function (pi) {
  pi.registerCommand('utils', { handler: async (_args, ctx) => {
    const jsonl = [
      JSON.stringify({ type: 'session', version: 3, id: 's', timestamp: '2026-01-01T00:00:00.000Z', cwd: ctx.cwd }),
      'not json',
      JSON.stringify({ type: 'message', id: 'a', parentId: null, timestamp: '2026-01-01T00:00:01.000Z',
        message: { role: 'user', content: 'first', timestamp: 1 } }),
      JSON.stringify({ type: 'message', id: 'b', parentId: 'a', timestamp: '2026-01-01T00:00:02.000Z',
        message: { role: 'assistant', content: [{ type: 'text', text: 'reply' }], provider: 'p', model: 'm',
          stopReason: 'stop', usage: { input: 5, output: 3, cacheRead: 0, cacheWrite: 0, totalTokens: 8 }, timestamp: 2 } }),
      JSON.stringify({ type: 'compaction', id: 'c', parentId: 'b', timestamp: '2026-01-01T00:00:03.000Z',
        summary: 'sum', firstKeptEntryId: 'b', tokensBefore: 10 }),
      JSON.stringify({ type: 'message', id: 'd', parentId: 'c', timestamp: '2026-01-01T00:00:04.000Z',
        message: { role: 'user', content: 'second', timestamp: 4 } }),
    ].join('\n');
    const entries = ca.parseSessionEntries(jsonl);
    const sessionEntries = entries.filter((entry) => entry.type !== 'session');
    const legacy = [{ type: 'session', version: 1 }, { type: 'message', message: { role: 'hookMessage', content: 'x' } }];
    ca.migrateSessionEntries(legacy);
    const context = ca.buildSessionContext(sessionEntries);

    const skillDir = join(ctx.cwd, 'skills', 'demo');
    mkdirSync(skillDir, { recursive: true });
    writeFileSync(join(skillDir, 'SKILL.md'), '---\nname: demo\ndescription: Demo <skill>\n---\nBody\n');
    const skills = ca.loadSkillsFromDir({ dir: join(ctx.cwd, 'skills'), source: 'path' });

    const pngPath = join(ctx.cwd, 'tiny.png');
    writeFileSync(pngPath, Buffer.from(PNG, 'base64'));
    const resized = await ca.resizeImage(Buffer.from(PNG, 'base64'), 'image/png', { maxWidth: 2 });
    const unsupportedSession = code(() => new ca.AgentSession());
    const definition = { name: 'echo', label: 'Echo', description: 'd', parameters: {},
      execute: async (id, params, _signal, _onUpdate, toolCtx) => `${id}:${params.v}:${toolCtx.tag}` };
    const runner = { createToolContext: (id) => ({ tag: `ctx-${id}` }) };
    const [wrapped] = ca.wrapRegisteredTools([{ definition }], runner);

    pi.appendEntry('utils', {
      count: entries.length,
      migrated: [legacy[0].version, legacy[1].message.role, typeof legacy[1].id],
      context: context.messages.map((message) => message.role),
      model: context.model,
      latest: ca.getLatestCompactionEntry(sessionEntries)?.id,
      usage: ca.getLastAssistantUsage(sessionEntries)?.totalTokens,
      cut: ca.findCutPoint(sessionEntries, 0, sessionEntries.length, 1),
      turnStart: ca.findTurnStartIndex(sessionEntries, 1, 0),
      source: ca.createSyntheticSourceInfo('<inline:x>', { source: 'inline' }),
      skillNames: skills.skills.map((skill) => skill.name),
      prompt: ca.formatSkillsForPrompt(skills.skills).includes('<description>Demo &lt;skill&gt;</description>'),
      diff: ca.generateDiffString('a\nb\n', 'a\nc\n'),
      patch: ca.generateUnifiedPatch('f.txt', 'a\n', 'b\n').includes('+b'),
      contextLines: code(() => ca.generateDiffString('a', 'b', 2)),
      mime: await ca.detectSupportedImageMimeTypeFromFile(pngPath),
      png: (await ca.convertToPng(PNG, 'image/png')).mimeType,
      resized: resized && [resized.originalWidth, resized.originalHeight, resized.width, resized.height, resized.wasResized],
      note: ca.formatDimensionNote({ wasResized: true, originalWidth: 4, originalHeight: 2, width: 2, height: 1 }),
      unsupported: [code(() => ca.parseArgs([])), unsupportedSession],
      wrapped: [wrapped.name, await wrapped.execute('t1', { v: 2 })],
      rendered: typeof ca.renderDiff('+1 added\n-1 removed\n') === 'string',
      credentialError: new ca.CredentialSynchronizationError('p', 'login', undefined, {}).message,
    });
  }});
}
"#;

const AI: &str = r#"
import * as ai from '@earendil-works/pi-ai';
import * as oauth from '@earendil-works/pi-ai/oauth';

const code = (fn) => { try { fn(); return 'none'; } catch (error) { return `${error.code}: ${error.message}`; } };

export default function (pi) {
  pi.registerCommand('ai', { handler: async () => {
    const tool = { name: 't', description: 'd', parameters: { type: 'object', properties: {} } };
    const messages = [
      { role: 'system', content: 'one', toolsAdded: [tool], timestamp: 0 },
      { role: 'user', content: 'hi', timestamp: 1 },
      { role: 'system', content: 'two', timestamp: 2 },
    ];
    const args = new ai.ToolCallArgumentsStream();
    const store = new ai.InMemoryModelsStore();
    await store.write('p', { models: [] });
    const retry = await ai.retryAssistantCall(async () => ai.fauxAssistantMessage('ok'), { enabled: true, maxRetries: 1, baseDelayMs: 1 });
    pi.appendEntry('ai', {
      systemPrompt: ai.getCurrentSystemPrompt(messages),
      tools: ai.getCurrentTools(messages).map((entry) => entry.name),
      collapsed: ai.collapseSystemMessages({ messages }).messages.map((message) => message.role),
      argsStream: typeof args.push === 'function' || typeof args.append === 'function' || typeof args.feed === 'function',
      identity: ai.mergeClientIdentity({ userAgent: 'short' }, { attribution: ai.PI_ATTRIBUTION }),
      userAgent: ai.formatUserAgent({ userAgent: 'short' }),
      delay: ai.retryDelayMs({ baseDelayMs: 100 }, 3),
      retryable: ai.isRetryableAssistantError({ stopReason: 'error', errorMessage: 'overloaded_error' }),
      retry: retry.content[0].text,
      modelsError: new ai.ModelsError('auth', 'bad').code,
      modelType: ai.getModelType({}),
      stored: (await store.read('p')).models.length,
      legacy: typeof ai.streamAnthropic === 'function' && typeof ai.streamSimpleOpenAIResponses === 'function',
      anthropicEnv: ai.ANTHROPIC_API_KEY_ENV,
      createModels: typeof ai.createModels().setProvider,
      oauthKeys: Object.keys(oauth).filter((key) => key !== 'default'),
    });
  }});
}
"#;

const CATALOG: &str = r#"
import * as all from '@earendil-works/pi-ai/providers/all';
import * as legacyAll from '@mariozechner/pi-ai/providers/all';
import * as compat from '@earendil-works/pi-ai/compat';
import * as models from '@earendil-works/pi-ai/models';
import { getSelectPageIndex, replaceControlCharacters } from '@earendil-works/pi-tui';
import * as legacyTui from '@mariozechner/pi-tui';

const NAMES = ['builtinModels', 'builtinProviders', 'getAllBuiltinModels', 'getBuiltinClassifierModel',
  'getBuiltinClassifierModels', 'getBuiltinImageModel', 'getBuiltinImageModels', 'getBuiltinModelDataGeneratedAt',
  'radiusProvider'];
const counts = (p) => [all.getBuiltinModels(p).length, all.getBuiltinImageModels(p).length,
  all.getBuiltinClassifierModels(p).length, all.getAllBuiltinModels(p).length];
const unused = () => { throw new Error('unused'); };

export default function (pi) {
  pi.registerCommand('catalog', { handler: async () => {
    const signal = new AbortController().signal;
    const providers = all.builtinProviders();
    const byId = (id) => providers.find((provider) => provider.id === id);
    const [groq, typesafe, anthropic] = ['groq', 'typesafe', 'anthropic'].map(byId);
    const groqModel = all.getBuiltinModels('groq')[0];
    const classifierId = all.getBuiltinClassifierModels('typesafe')[0].id;
    const registry = all.builtinModels();
    const custom = models.createModels();
    custom.setProvider(models.createProvider({ id: 'local', models: [{ ...groqModel, provider: 'local' }],
      auth: { apiKey: { name: 'k', resolve: async () => ({ auth: { apiKey: 'x' } }) } },
      api: { stream: unused, streamSimple: unused } }));
    const groqCtx = (key) => ({ env: async (name) => (name === 'GROQ_API_KEY' ? key : undefined), fileExists: async () => false });
    process.env.GROQ_API_KEY = 'host-key';
    const auth = {
      stored: await groq.auth.apiKey.resolve({ ctx: groqCtx('ctx-key'), credential: { type: 'api_key', key: 'k1' }, signal }),
      ctx: await groq.auth.apiKey.resolve({ ctx: groqCtx('ctx-key'), signal }),
      empty: (await groq.auth.apiKey.resolve({ ctx: groqCtx(undefined), signal })) ?? null,
      isolated: (await all.builtinModels({ authContext: groqCtx(undefined) }).getAuth('groq')) ?? null,
      process: (await all.builtinModels().getAuth('groq'))?.auth ?? null,
    };
    delete process.env.GROQ_API_KEY;
    const prompts = [];
    const interaction = { signal, notify: () => {}, prompt: async (prompt) => (prompts.push(prompt), 'nope') };
    const login = await all.radiusProvider({ gateway: 'http://127.0.0.1:9' }).auth.oauth.login(interaction)
      .then(() => 'ok', (error) => error.message);
    const kb = { matches: (data, id) => (data === 'up' && id === 'tui.select.pageUp') || (data === 'down' && id === 'tui.select.pageDown') };
    pi.appendEntry('catalog', {
      missing: NAMES.filter((name) => typeof all[name] !== 'function' || typeof legacyAll[name] !== 'function'),
      schema: [compat.AI_TELEMETRY_SCHEMA.version, models.AI_TELEMETRY_SCHEMA === compat.AI_TELEMETRY_SCHEMA,
        Object.keys(models.AI_TELEMETRY_SCHEMA.spans)],
      chatOnly: compat.getModels('openrouter').every((model) => model.type === 'chat'),
      counts: ['openrouter', 'typesafe'].map(counts),
      types: [all.getBuiltinImageModel('openrouter', all.getBuiltinImageModels('openrouter')[0].id).type,
        all.getBuiltinClassifierModel('typesafe', classifierId).type, all.getBuiltinModel('typesafe', classifierId) ?? null,
        all.getBuiltinModels().length],
      generatedAt: new Date(all.getBuiltinModelDataGeneratedAt()).toISOString(),
      providerIds: providers.map((provider) => provider.id),
      groq: [groq.name, groq.baseUrl, groq.getModels().length === all.getBuiltinModels('groq').length, groq.auth.apiKey.name],
      typesafe: typesafe.getAllModels().map((model) => model.type),
      auth,
      oauth: [anthropic.auth.oauth.name, anthropic.auth.oauth.isSubscription,
        await anthropic.auth.oauth.toAuth({ type: 'oauth', access: 'tok', refresh: 'r', expires: 1 })],
      login: [login, prompts.map((prompt) => [prompt.type, prompt.message, prompt.options.map((option) => option.id)])],
      registry: [registry.getProviders().length, registry.getModel('groq', groqModel.id).provider,
        registry.getModelsOfType('classifier', 'typesafe').length],
      custom: custom.getModels('local').map((model) => model.provider),
      radius: [all.radiusProvider().getModels().length === all.getBuiltinModels('radius').length,
        all.radiusProvider({ id: 'r2' }).getModels().every((model) => model.provider === 'r2'),
        all.radiusProvider({ gateway: 'gateway.example' }).getModels().length],
      tui: [replaceControlCharacters('a\x1b[31m\nb\x07'), getSelectPageIndex(kb, 'down', 2, 10, 5),
        getSelectPageIndex(kb, 'down', 8, 10, 5), getSelectPageIndex(kb, 'up', 2, 10, 5),
        getSelectPageIndex(kb, 'x', 2, 10, 5) ?? null, typeof legacyTui.getSelectPageIndex],
    });
  }});
}
"#;

const OPERATIONS: &str = r#"
import { builtinModels, getBuiltinClassifierModels, getBuiltinImageModels, radiusProvider } from '@earendil-works/pi-ai/providers/all';

const KEYS = { TYPESAFE_API_KEY: 'ts-key', OPENROUTER_API_KEY: 'or-key' };
const authContext = { env: async (name) => KEYS[name], fileExists: async () => false };

export default function (pi) {
  pi.registerCommand('operations', { handler: async () => {
    const models = builtinModels({ authContext });
    const classifier = { ...getBuiltinClassifierModels('typesafe')[0], baseUrl: '__BASE__/v1/' };
    const image = { ...getBuiltinImageModels('openrouter')[0], baseUrl: '__BASE__/api/v1' };
    const questions = { approved: { type: 'bool', instructions: 'Approved?', criteria: { true: 'Yes', false: 'No' } } };
    const classified = await models.classify(classifier, { state: { text: 'Ship it.' }, questions }, {});
    const images = await models.generateImages(image, { input: [{ type: 'text', text: 'Draw a dog' }] }, {});
    const oauth = radiusProvider({ gateway: '__BASE__' }).auth.oauth;
    const refreshed = await oauth.refresh({ type: 'oauth', access: 'old', refresh: 'r1', expires: 1 }, new AbortController().signal);
    pi.appendEntry('operations', {
      classified: [classified.provider, classified.stopReason, classified.answers.approved],
      images: [images.provider, images.stopReason, images.responseId, images.output.map((block) => block.type)],
      refreshed: [refreshed.type, refreshed.access, refreshed.refresh, refreshed.expires > Date.now()],
    });
  }});
}
"#;

const VERTEX: &str = r#"
import { builtinProviders } from '@earendil-works/pi-ai/providers/all';

const PLACE = { GOOGLE_CLOUD_PROJECT: 'p', GOOGLE_CLOUD_LOCATION: 'l' };

export default function (pi) {
  pi.registerCommand('vertex', { handler: async () => {
    const vertex = builtinProviders().find((provider) => provider.id === 'google-vertex');
    const signal = new AbortController().signal;
    const resolve = async (vars, exists, credential) => {
      const asked = [];
      const ctx = { env: async (name) => vars[name], fileExists: async (path) => (asked.push(path), exists) };
      const result = await vertex.auth.apiKey.resolve({ ctx, credential, signal });
      return [result?.source ?? null, asked];
    };
    pi.appendEntry('vertex', {
      hostFileCtxFalse: await resolve({ ...PLACE, GOOGLE_APPLICATION_CREDENTIALS: '__HOST_ADC__' }, false),
      missingFileCtxTrue: await resolve({ ...PLACE, GOOGLE_APPLICATION_CREDENTIALS: '/no/such/adc.json' }, true),
      defaultCtxTrue: await resolve(PLACE, true),
      defaultCtxFalse: await resolve(PLACE, false),
      credentialPath: await resolve(PLACE, true, { type: 'api_key', env: { GOOGLE_APPLICATION_CREDENTIALS: '/cred/adc.json' } }),
    });
  }});
}
"#;

const AGENT: &str = r#"
import { Agent, agentLoop, streamProxy } from '@earendil-works/pi-agent-core';

export default function (pi) {
  pi.registerCommand('agent', { handler: async (_args, ctx) => {
    const agent = new Agent({ initialState: { model: ctx.model, systemPrompt: 'Be brief.', thinkingLevel: 'off' } });
    const events = [];
    agent.subscribe((event) => events.push(event.type));
    await agent.prompt('agent question');
    pi.appendEntry('agent', {
      roles: agent.state.messages.map((message) => message.role),
      last: agent.state.messages.at(-1),
      text: agent.state.messages.at(-1).content.map((block) => block.text ?? '').join(''),
      events: [...new Set(events)],
      exports: [typeof agentLoop, typeof streamProxy],
    });
  }});
}
"#;

async fn run(name: &'static str, source: String, command: &str, responses: Vec<FauxResponse>) -> (Harness, Value) {
    let h = harness(Setup { files: vec![(name, source)], load: vec![name], responses, ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let text = format!("/{command}");
    let prompt = h.session.prompt(&text, Vec::new());
    tokio::time::timeout(std::time::Duration::from_secs(20), prompt).await.unwrap().unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entry = h.entries().into_iter().find(|entry| entry["customType"] == command);
    let data = entry.map(|entry| entry["data"].clone()).unwrap_or_else(|| panic!("no {command} entry"));
    (h, data)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coding_agent_utilities_run_upstream_code_and_host_operations() {
    let (_h, data) = run("utils.ts", UTILITIES.replace("__PNG__", PNG_4X2), "utils", Vec::new()).await;
    assert_eq!(data["count"], 5);
    assert_eq!(data["migrated"], json!([3, "custom", "string"]));
    assert_eq!(data["context"], json!(["compactionSummary", "assistant", "user"]));
    assert_eq!(data["model"], json!({"provider": "p", "modelId": "m"}));
    assert_eq!(data["latest"], "c");
    assert_eq!(data["usage"], 8);
    assert_eq!(data["cut"]["firstKeptEntryIndex"], 3);
    assert_eq!(data["turnStart"], 0);
    assert_eq!(
        data["source"],
        json!({"path": "<inline:x>", "source": "inline", "scope": "temporary", "origin": "top-level"})
    );
    assert_eq!(data["skillNames"], json!(["demo"]));
    assert_eq!(data["prompt"], true);
    assert_eq!(data["diff"]["firstChangedLine"], 2);
    assert!(data["diff"]["diff"].as_str().unwrap().contains('c'), "{data}");
    assert_eq!(data["patch"], true);
    assert_eq!(
        data["contextLines"],
        "ERR_PI_UNSUPPORTED: generateDiffString contextLines other than 4 is not supported in pi-rust"
    );
    assert_eq!(data["mime"], "image/png");
    assert_eq!(data["png"], "image/png");
    assert_eq!(data["resized"], json!([4, 2, 2, 1, true]));
    assert!(data["note"].as_str().unwrap().contains("Multiply coordinates by 2.00"));
    assert_eq!(
        data["unsupported"],
        json!([
            "ERR_PI_UNSUPPORTED: parseArgs is not supported in pi-rust",
            "ERR_PI_UNSUPPORTED: AgentSession is not supported in pi-rust"
        ])
    );
    assert_eq!(data["wrapped"], json!(["echo", "t1:2:ctx-t1"]));
    assert_eq!(data["rendered"], true);
    assert_eq!(data["credentialError"], "Credential login committed for p, but local synchronization failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pi_ai_exports_embedded_utilities_and_type_only_oauth() {
    let (_h, data) = run("ai.ts", AI.into(), "ai", Vec::new()).await;
    assert_eq!(data["systemPrompt"], "one\n\ntwo");
    assert_eq!(data["tools"], json!(["t"]));
    assert_eq!(data["collapsed"], json!(["system", "user"]));
    assert_eq!(data["argsStream"], true);
    assert_eq!(data["identity"]["userAgent"], "short");
    assert_eq!(data["identity"]["attribution"]["name"], "pi");
    assert_eq!(data["userAgent"], "pi");
    assert_eq!(data["delay"], 400);
    assert_eq!(data["retryable"], true);
    assert_eq!(data["retry"], "ok");
    assert_eq!(data["modelsError"], "auth");
    assert_eq!(data["modelType"], "chat");
    assert_eq!(data["stored"], 0);
    assert_eq!(data["legacy"], true);
    assert_eq!(data["anthropicEnv"], "ANTHROPIC_API_KEY");
    assert_eq!(data["createModels"], "function");
    assert_eq!(data["oauthKeys"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_core_runs_a_prompt_on_the_default_stream_function() {
    let (h, data) = run("agent.ts", AGENT.into(), "agent", vec![FauxResponse::text("agent answer")]).await;
    assert_eq!(data["roles"], json!(["system", "user", "assistant"]), "{data}");
    assert_eq!(data["text"], "agent answer");
    for event in ["agent_start", "message_end", "agent_end"] {
        assert!(data["events"].as_array().unwrap().contains(&json!(event)), "{data}");
    }
    assert_eq!(data["exports"], json!(["function", "function"]));
    assert_eq!(h.provider.requests().len(), 1);
}

/// Models of `provider` by type in the bundled catalog: chat, image, classifier, and all.
fn catalog_counts(provider: &str) -> Value {
    let models: Vec<_> = pi_ai::models::builtin_models().iter().filter(|model| model.provider == provider).collect();
    let of = |kind: &str| models.iter().filter(|model| pi_ai::model_operations::model_type(model) == kind).count();
    json!([of("chat"), of("image"), of("classifier"), models.len()])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn providers_all_reads_the_rust_catalog_and_builds_upstream_providers() {
    let (_h, data) = run("catalog.ts", CATALOG.into(), "catalog", Vec::new()).await;
    assert_eq!(data["missing"], json!([]), "{data}");
    assert_eq!(data["schema"], json!([1, true, ["pi.ai.request", "pi.ai.poll"]]));
    assert_eq!(data["chatOnly"], true);
    assert_eq!(data["counts"], json!([catalog_counts("openrouter"), catalog_counts("typesafe")]));
    assert_eq!(data["types"], json!(["image", "classifier", null, 0]));
    assert_eq!(data["generatedAt"], pi_ai::models::builtin_model_data_generated_at().unwrap());
    let ids = data["providerIds"].as_array().unwrap();
    assert_eq!((ids.len(), &ids[0]), (42, &json!("amazon-bedrock")), "{data}");
    assert!(ids.contains(&json!("radius")) && !ids.contains(&json!("cursor")), "{data}");
    assert_eq!(data["groq"], json!(["Groq", "https://api.groq.com/openai/v1", true, "Groq API key"]));
    assert_eq!(data["typesafe"], json!(["classifier"]));
    let auth = &data["auth"];
    assert_eq!(auth["stored"], json!({"auth": {"apiKey": "k1"}, "source": "stored credential"}), "{data}");
    assert_eq!(auth["ctx"], json!({"auth": {"apiKey": "ctx-key"}, "source": "GROQ_API_KEY"}), "{data}");
    assert_eq!((&auth["empty"], &auth["isolated"]), (&Value::Null, &Value::Null), "{data}");
    assert_eq!(auth["process"], json!({"apiKey": "host-key"}), "{data}");
    assert_eq!(data["oauth"], json!(["Anthropic (Claude Pro/Max)", true, {"apiKey": "tok"}]));
    assert!(data["login"][0].as_str().unwrap().contains("Unknown Radius sign-in method: nope"), "{data}");
    assert_eq!(data["login"][1], json!([["select", "Sign in to Radius:", ["browser", "device-code"]]]));
    assert_eq!(data["registry"], json!([42, "groq", 1]));
    assert_eq!(data["custom"], json!(["local"]));
    assert_eq!(data["radius"], json!([true, true, 0]));
    assert_eq!(data["tui"], json!(["a [31m b ", 7, 9, 0, null, "function"]));
}

/// A local server answering each request with the next JSON body; it records the path, the
/// `authorization` header, and the body (JSON, else text) of every request.
async fn json_server(bodies: Vec<Value>) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = requests.clone();
    tokio::spawn(async move {
        for body in bodies {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let (mut buffer, mut chunk) = (Vec::new(), [0u8; 8192]);
            let (head, request) = loop {
                let read = socket.read(&mut chunk).await.unwrap();
                buffer.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&buffer).to_string();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let length = head.lines().find_map(|line| {
                        line.to_lowercase().strip_prefix("content-length:").map(|value| value.trim().parse().unwrap())
                    });
                    if rest.len() >= length.unwrap_or(0) || read == 0 {
                        break (head.to_string(), rest.to_string());
                    }
                }
            };
            let header = |name: &str| {
                head.lines()
                    .find_map(|line| line.to_lowercase().strip_prefix(name).map(|value| value.trim().to_string()))
            };
            recorded.lock().unwrap().push(json!({
                "path": head.split_whitespace().nth(1),
                "authorization": header("authorization:"),
                "body": serde_json::from_str::<Value>(&request).unwrap_or(Value::String(request)),
            }));
            let body = body.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = socket.shutdown().await;
        }
    });
    (base, requests)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builtin_providers_classify_generate_images_and_refresh_through_rust() {
    let classified = json!({"answers": {"approved": {"type": "noul", "noul": 0.95}}});
    let images = json!({"id": "img-1", "choices": [{"message": {"content": "A dog.",
        "images": [{"image_url": "data:image/png;base64,ZmFrZS1wbmc="}]}}]});
    let token = json!({"access_token": "new", "refresh_token": "r2", "expires_in": 3600});
    let (base, requests) = json_server(vec![classified, images, token]).await;
    let (_h, data) = run("operations.ts", OPERATIONS.replace("__BASE__", &base), "operations", Vec::new()).await;
    assert_eq!(data["classified"], json!(["typesafe", "stop", {"type": "bool", "probability": 0.95}]), "{data}");
    assert_eq!(data["images"], json!(["openrouter", "stop", "img-1", ["text", "image"]]), "{data}");
    let requests = requests.lock().unwrap().clone();
    assert_eq!(
        (&requests[0]["path"], &requests[0]["authorization"]),
        (&json!("/v1/systemone"), &json!("bearer ts-key"))
    );
    assert_eq!(requests[0]["body"]["questions"]["approved"]["type"], "noul");
    assert_eq!(
        (&requests[1]["path"], &requests[1]["authorization"]),
        (&json!("/api/v1/chat/completions"), &json!("bearer or-key"))
    );
    assert_eq!(requests[1]["body"]["messages"][0]["content"][0]["text"], "Draw a dog");
    assert_eq!(data["refreshed"], json!(["oauth", "new", "r2", true]), "{data}");
    assert_eq!(requests[2]["path"], "/v1/oauth/token");
    let form = requests[2]["body"].as_str().unwrap();
    assert!(form.contains("grant_type=refresh_token") && form.contains("refresh_token=r1"), "{form}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vertex_auth_asks_the_caller_context_for_the_adc_file() {
    let dir = tempfile::tempdir().unwrap();
    let host_adc = dir.path().join("adc.json");
    std::fs::write(&host_adc, "{}").unwrap();
    let source = VERTEX.replace("__HOST_ADC__", &host_adc.display().to_string().replace('\\', "/"));
    let (_h, data) = run("vertex.ts", source, "vertex", Vec::new()).await;
    let adc = "~/.config/gcloud/application_default_credentials.json";
    let ambient = "ambient credentials";
    assert_eq!(data["hostFileCtxFalse"][0], Value::Null, "{data}");
    assert_eq!(data["missingFileCtxTrue"], json!([ambient, ["/no/such/adc.json"]]), "{data}");
    assert_eq!(data["defaultCtxTrue"], json!([ambient, [adc]]), "{data}");
    assert_eq!(data["defaultCtxFalse"], json!([null, [adc]]), "{data}");
    assert_eq!(data["credentialPath"], json!([ambient, ["/cred/adc.json"]]), "{data}");
}

const SUBSCRIPTION: &str = r#"
import { ModelRuntime } from '@earendil-works/pi-coding-agent';

export default function (pi) {
  pi.registerCommand('subscription', { handler: async () => {
    const runtime = await ModelRuntime.create();
    const providers = ['anthropic', 'openrouter', 'openai', 'groq'];
    pi.appendEntry('subscription', providers.map((provider) => runtime.isUsingSubscription(provider)));
  }});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_runtime_reports_subscriptions_from_stored_oauth() {
    use pi_ai::auth::{ApiKeyCredential, Credential, CredentialStore, FileCredentialStore, OAuthCredential};
    let setup = Setup {
        files: vec![("subscription.ts", SUBSCRIPTION.into())],
        load: vec!["subscription.ts"],
        ..Setup::default()
    };
    let h = harness(setup).await;
    let oauth = |access: &str| {
        Credential::OAuth(OAuthCredential {
            refresh: "r".into(),
            access: access.into(),
            expires: i64::MAX,
            ..Default::default()
        })
    };
    // Anthropic's OAuth flow is a subscription, OpenRouter's is not; openai has an API key, groq nothing.
    let stored = [
        ("anthropic", oauth("a")),
        ("openrouter", oauth("o")),
        ("openai", Credential::ApiKey(ApiKeyCredential { key: Some("sk".into()), env: None })),
    ];
    let store = FileCredentialStore::new(h.agent_dir.join("auth.json"));
    for (provider, credential) in stored {
        store.update(provider, Box::new(move |_| Box::pin(async move { Ok(Some(credential)) }))).await.unwrap();
    }
    let prompt = h.session.prompt("/subscription", Vec::new());
    tokio::time::timeout(std::time::Duration::from_secs(20), prompt).await.unwrap().unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entry =
        h.entries().into_iter().find(|entry| entry["customType"] == "subscription").expect("subscription entry");
    assert_eq!(entry["data"], json!([true, false, false, false]));
}

const AI_COMPAT: &str = r#"
import * as ai from '@earendil-works/pi-ai';
import { Agent } from '@earendil-works/pi-agent-core';
import { scenario } from './ai_compat_scenario.mjs';

export default function (pi) {
  pi.registerCommand('compat', { handler: async () => {
    const result = await scenario(ai);
    const image = { id: 'img-or', name: 'OR', api: 'openrouter-images', provider: 'openrouter', type: 'image',
      input: ['text'], baseUrl: '__BASE__/api/v1' };
    const drawn = await ai.generateImagesOpenRouter(image, { input: [{ type: 'text', text: 'a dog' }] }, { apiKey: 'or-key' });
    const faux = ai.registerFauxProvider({ api: 'faux-agent', provider: 'faux-agent', models: [{ id: 'agent-model' }] });
    faux.setResponses([ai.fauxAssistantMessage('from the faux provider')]);
    const agent = new Agent({ initialState: { model: faux.getModel(), systemPrompt: 'Be brief.', thinkingLevel: 'off' } });
    await agent.prompt('hello');
    faux.unregister();
    pi.appendEntry('compat', {
      ...result,
      drawn: [drawn.stopReason, drawn.responseId, drawn.output.map((block) => block.type)],
      agent: agent.state.messages.at(-1).content.map((block) => block.text).join(''),
      catalog: [ai.getImageProviders(), ai.getImageModels('openrouter').length, ai.getImageModel('openrouter', ai.getImageModels('openrouter')[0].id)?.type],
    });
  }});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compat_faux_provider_and_image_api_match_upstream() {
    let images = json!({"id": "img-1", "choices": [{"message": {"content": "A dog.",
        "images": [{"image_url": "data:image/png;base64,ZmFrZS1wbmc="}]}}]});
    let (base, requests) = json_server(vec![images]).await;
    let files = vec![
        ("compat.ts", AI_COMPAT.replace("__BASE__", &base)),
        ("ai_compat_scenario.mjs", include_str!("fixtures/extensions/ai_compat_scenario.mjs").to_string()),
    ];
    let h = harness(Setup { files, load: vec!["compat.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let prompt = h.session.prompt("/compat", Vec::new());
    tokio::time::timeout(std::time::Duration::from_secs(20), prompt).await.unwrap().unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entry = h.entries().into_iter().find(|entry| entry["customType"] == "compat").expect("compat entry");
    let data = &entry["data"];

    let expected: Value = serde_json::from_str(include_str!("fixtures/extensions/ai_compat_scenario.json")).unwrap();
    assert_eq!(data["faux"], expected["faux"], "{data}");
    assert_eq!(data["images"], expected["images"], "{data}");

    assert_eq!(data["drawn"], json!(["stop", "img-1", ["text", "image"]]), "{data}");
    let requests = requests.lock().unwrap().clone();
    assert_eq!(
        (&requests[0]["path"], &requests[0]["authorization"]),
        (&json!("/api/v1/chat/completions"), &json!("bearer or-key"))
    );
    assert_eq!(data["agent"], "from the faux provider");

    let with_images: Vec<String> = {
        let mut providers: Vec<String> = pi_ai::models::builtin_models()
            .iter()
            .filter(|model| pi_ai::model_operations::model_type(model) == "image")
            .map(|model| model.provider.clone())
            .collect();
        providers.sort();
        providers.dedup();
        providers
    };
    let openrouter_images = catalog_counts("openrouter")[1].clone();
    assert_eq!(data["catalog"], json!([with_images, openrouter_images, "image"]), "{data}");
}

const CAPTURE: &str = r#"
import * as ai from '@earendil-works/pi-ai';

export default function (pi) {
  pi.registerCommand('capture', { handler: async () => {
    const seen = [];
    const run = (_model, context, options) => {
      seen.push({ context, apiKey: options?.apiKey ?? null });
      const stream = ai.createAssistantMessageEventStream();
      const message = ai.fauxAssistantMessage('ok');
      stream.push({ type: 'done', reason: 'stop', message });
      stream.end(message);
      return stream;
    };
    ai.registerApiProvider({ api: 'capture', stream: run, streamSimple: run }, 'capture-test');
    delete process.env.GROQ_API_KEY;
    const model = { id: 'm', name: 'm', api: 'capture', provider: 'groq', baseUrl: 'http://127.0.0.1:9', input: ['text'],
      reasoning: false, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 1000, maxTokens: 100 };
    const context = { systemPrompt: 'Be brief', messages: [{ role: 'user', content: 'hi', timestamp: 1 }],
      tools: [{ name: 't', description: 'd', parameters: { type: 'object', properties: {} } }] };
    await ai.complete(model, context, { env: { GROQ_API_KEY: 'env-key' } });
    await ai.completeSimple(model, context, { apiKey: 'explicit', env: { GROQ_API_KEY: 'env-key' } });
    await ai.complete(model, context, { env: {} });
    ai.unregisterApiProviders('capture-test');
    pi.appendEntry('capture', seen);
  }});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_api_providers_get_the_normalized_context_and_env_key() {
    let (_h, data) = run("capture.ts", CAPTURE.into(), "capture", Vec::new()).await;
    let tool = json!({"name": "t", "description": "d", "parameters": {"type": "object", "properties": {}}});
    // Upstream `normalizeContext` output for the same context, from the pinned source under Node.
    let normalized = json!({"messages": [
        {"role": "system", "content": "Be brief", "toolsAdded": [tool], "timestamp": 0},
        {"role": "user", "content": "hi", "timestamp": 1},
    ]});
    assert_eq!(data[0]["context"], normalized, "{data}");
    let keys: Vec<&Value> = data.as_array().unwrap().iter().map(|call| &call["apiKey"]).collect();
    assert_eq!(keys, [&json!("env-key"), &json!("explicit"), &Value::Null], "{data}");
}
