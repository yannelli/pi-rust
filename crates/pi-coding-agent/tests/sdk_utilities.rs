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
      unsupported: code(() => ai.createModels()),
      oauthKeys: Object.keys(oauth).filter((key) => key !== 'default'),
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
    assert_eq!(data["unsupported"], "ERR_PI_UNSUPPORTED: createModels is not supported in pi-rust");
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
