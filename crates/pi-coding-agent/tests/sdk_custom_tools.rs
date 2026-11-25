//! SDK `createAgentSession({ customTools })`: definitions run in the caller's host with a context
//! of the child session, and win over extension tools and built-in tools of the same name.

use crate::extensions::harness;

use harness::{Harness, Setup, harness};
use pi_ai::providers::faux::FauxResponse;
use serde_json::{Value, json};

const PRELUDE: &str = r#"
import { createAgentSession, DefaultResourceLoader, getAgentDir, SessionManager, SettingsManager } from '@earendil-works/pi-coding-agent';
const settings = () => SettingsManager.inMemory({ retry: { enabled: false }, compaction: { enabled: false } });
const text = (value) => ({ content: [{ type: 'text', text: value }], details: {} });
"#;

async fn run(body: &str, responses: Vec<FauxResponse>) -> (Harness, Value) {
    let source = format!(
        "{PRELUDE}\nexport default pi => pi.registerCommand('custom', {{ handler: async (_args, ctx) => {{\n{body}\n}}}});\n"
    );
    let h =
        harness(Setup { files: vec![("custom.ts", source)], load: vec!["custom.ts"], responses, ..Setup::default() })
            .await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    tokio::time::timeout(std::time::Duration::from_secs(30), h.session.prompt("/custom", Vec::new()))
        .await
        .expect("the command finishes")
        .unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let data = h.entries().iter().find(|entry| entry["customType"] == "result").expect("result entry")["data"].clone();
    (h, data)
}

fn tool_result(messages: &Value, name: &str) -> Value {
    let found = messages
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "toolResult" && message["toolName"] == name);
    found.cloned().unwrap_or(Value::Null)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_session_calls_a_custom_tool_in_the_caller_host() {
    let body = r#"
  const calls = [];
  const manager = SessionManager.inMemory(ctx.cwd);
  const echo = {
    name: 'echo_sdk', label: 'Echo', description: 'Echo the text', promptSnippet: 'Echo text back',
    parameters: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] },
    prepareArguments: (args) => ({ ...args, text: `${args.text}!` }),
    async execute(toolCallId, params, signal, onUpdate, toolCtx) {
      onUpdate?.(text('working'));
      calls.push({ toolCallId, params, aborted: signal?.aborted ?? null, cwd: toolCtx.cwd,
        sessionId: toolCtx.sessionManager.getSessionId(), tools: toolCtx.tools.map((tool) => tool.name) });
      return text(`echo: ${params.text}`);
    },
  };
  // As @tintinweb/pi-subagents: `modelRegistry` is not an upstream option, so it is ignored.
  const { session } = await createAgentSession({
    model: ctx.model, tools: ['echo_sdk'], customTools: [echo], sessionManager: manager, settingsManager: settings(),
    modelRegistry: ctx.modelRegistry, modelRuntime: ctx.modelRegistry.runtime,
  });
  const updates = [];
  session.subscribe((event) => { if (event.type === 'tool_execution_update') updates.push(event.partialResult.content[0].text); });
  const info = session.getAllTools().find((tool) => tool.name === 'echo_sdk');
  const active = session.getActiveToolNames();
  await session.prompt('use the tool');
  pi.appendEntry('result', { calls, updates, info, active, messages: session.messages, childId: manager.getSessionId(),
    parentId: ctx.sessionManager.getSessionId() });
  session.dispose();
"#;
    let responses =
        vec![FauxResponse::tool_call("call-1", "echo_sdk", json!({"text": "hi"})), FauxResponse::text("done")];
    let (h, data) = run(body, responses).await;
    let calls = data["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1, "{data}");
    assert_eq!(calls[0]["toolCallId"], "call-1");
    assert_eq!(calls[0]["params"], json!({"text": "hi!"}));
    assert_eq!(calls[0]["aborted"], false);
    assert_eq!(calls[0]["sessionId"], data["childId"]);
    assert_ne!(data["childId"], data["parentId"]);
    assert_eq!(calls[0]["tools"], json!(["echo_sdk"]));
    assert_eq!(data["updates"], json!(["working"]));
    assert_eq!(data["info"]["sourceInfo"], json!({"path": "<sdk:echo_sdk>", "source": "sdk"}));
    assert_eq!(data["active"], json!(["echo_sdk"]));
    assert_eq!(tool_result(&data["messages"], "echo_sdk")["content"][0]["text"], "echo: hi!");
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), ["echo_sdk"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_tools_win_over_extension_and_built_in_tools() {
    let body = r#"
  const ran = [];
  const custom = (name) => ({
    name, label: name, description: `custom ${name}`, parameters: { type: 'object', properties: {} },
    async execute() { ran.push(`custom ${name}`); return text(`custom ${name}`); },
  });
  const loader = new DefaultResourceLoader({
    cwd: ctx.cwd, agentDir: getAgentDir(), settingsManager: settings(), noExtensions: true, noSkills: true,
    noPromptTemplates: true, noContextFiles: true,
    extensionFactories: [(api) => api.registerTool({
      name: 'shared', label: 'shared', description: 'extension shared', parameters: { type: 'object', properties: {} },
      async execute() { ran.push('extension shared'); return text('extension shared'); },
    })],
  });
  await loader.reload();
  const { session } = await createAgentSession({
    model: ctx.model, tools: ['read', 'shared'], customTools: [custom('read'), custom('shared')],
    resourceLoader: loader, sessionManager: SessionManager.inMemory(ctx.cwd), settingsManager: loader.settingsManager,
  });
  const infos = session.getAllTools().filter((tool) => tool.name === 'read' || tool.name === 'shared')
    .map((tool) => [tool.name, tool.description, tool.sourceInfo.path]);
  await session.prompt('call both');
  pi.appendEntry('result', { ran, infos, messages: session.messages });
  session.dispose();
"#;
    let responses = vec![
        FauxResponse::tool_calls(&[("call-read", "read", json!({})), ("call-shared", "shared", json!({}))]),
        FauxResponse::text("done"),
    ];
    let (_h, data) = run(body, responses).await;
    let mut ran: Vec<String> = serde_json::from_value(data["ran"].clone()).unwrap();
    ran.sort();
    assert_eq!(ran, ["custom read", "custom shared"]);
    assert_eq!(
        data["infos"],
        json!([["read", "custom read", "<sdk:read>"], ["shared", "custom shared", "<sdk:shared>"]])
    );
    assert_eq!(tool_result(&data["messages"], "read")["content"][0]["text"], "custom read");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_tool_errors_reach_the_model_and_invalid_entries_throw() {
    let body = r#"
  const failing = {
    name: 'fails', label: 'fails', description: 'Always fails', parameters: { type: 'object', properties: {} },
    async execute() { throw new Error('custom failure'); },
  };
  const { session } = await createAgentSession({
    model: ctx.model, tools: ['fails'], customTools: [failing], sessionManager: SessionManager.inMemory(ctx.cwd),
    settingsManager: settings(),
  });
  await session.prompt('fail');
  let invalid;
  try { await createAgentSession({ model: ctx.model, customTools: [{ name: 'no_execute' }] }); }
  catch (error) { invalid = `${error.name}: ${error.message}`; }
  pi.appendEntry('result', { messages: session.messages, invalid });
  session.dispose();
"#;
    let responses = vec![FauxResponse::tool_call("call-f", "fails", json!({})), FauxResponse::text("done")];
    let (_h, data) = run(body, responses).await;
    let result = tool_result(&data["messages"], "fails");
    assert_eq!(result["isError"], true);
    assert_eq!(result["content"][0]["text"], "custom failure");
    assert_eq!(
        data["invalid"],
        "TypeError: Each createAgentSession.customTools entry needs a name and an execute function"
    );
}
