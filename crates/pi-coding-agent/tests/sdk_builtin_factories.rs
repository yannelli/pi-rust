//! `createCodemodeExtension`, `createToolSearchExtension`, and `createMcpExtension`: the tools they
//! register run the native tools of the session, as a regular extension, captured by another
//! tool, and as factories of an SDK `DefaultResourceLoader`; the MCP factory starts native MCP.

use crate::extensions::harness;

use harness::{Harness, Setup, harness};
use pi_ai::providers::faux::FauxResponse;
use serde_json::{Value, json};

async fn start(source: &str, responses: Vec<FauxResponse>, settings: Value) -> Harness {
    let h = harness(Setup {
        files: vec![("factories.ts", source.into())],
        load: vec!["factories.ts"],
        responses,
        settings,
        root_files: vec![("work/note.txt", "note contents".into())],
        ..Setup::default()
    })
    .await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    h
}

async fn prompt(h: &Harness, text: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(30), h.session.prompt(text, Vec::new()))
        .await
        .expect("the prompt finishes")
        .unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
}

fn tool_text(h: &Harness, name: &str) -> String {
    let messages = serde_json::to_value(h.session.messages()).unwrap();
    let result = messages.as_array().unwrap().iter().find(|message| message["toolName"] == name).cloned();
    let blocks = result.map(|message| message["content"].as_array().cloned().unwrap_or_default()).unwrap_or_default();
    blocks.iter().filter_map(|block| block["text"].as_str()).collect::<Vec<_>>().join("\n")
}

fn entry(h: &Harness, kind: &str) -> Value {
    h.entries().iter().find(|entry| entry["customType"] == kind).expect("entry")["data"].clone()
}

const SCRIPT: &str = "const text = await tools.read({ path: 'note.txt' }); return text;";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_decorated_codemode_factory_runs_native_scripts_in_the_session() {
    let source = r#"
import { createCodemodeExtension } from '@earendil-works/pi-coding-agent';
export default function (pi) {
  const decorated = new Proxy(pi, { get(target, property, receiver) {
    if (property === 'registerTool') return (tool) => pi.registerTool({ ...tool, promptGuidelines: [...tool.promptGuidelines, 'Extra rule'] });
    return Reflect.get(target, property, receiver);
  } });
  createCodemodeExtension({ models: false })(decorated);
  pi.registerCommand('activate', { handler: async () => {
    const before = pi.getActiveTools();
    pi.setActiveTools([...before, 'codemode']);
    const info = pi.getAllTools().find((tool) => tool.name === 'codemode');
    pi.appendEntry('info', { before, info });
  } });
}
"#;
    let responses =
        vec![FauxResponse::tool_call("call-1", "codemode", json!({"code": SCRIPT})), FauxResponse::text("done")];
    let h = start(source, responses, json!({})).await;
    prompt(&h, "/activate").await;
    let info = entry(&h, "info");
    assert!(!info["before"].as_array().unwrap().contains(&json!("codemode")), "registered inactive: {info}");
    assert_eq!(info["info"]["exposure"], "model-only");
    assert!(info["info"]["sourceInfo"]["path"].as_str().unwrap().ends_with("factories.ts"));
    assert_eq!(info["info"]["promptGuidelines"].as_array().unwrap().last(), Some(&json!("Extra rule")));
    prompt(&h, "run a script").await;
    let text = tool_text(&h, "codemode");
    assert!(text.starts_with("Script completed"), "{text}");
    assert!(text.contains("note contents"), "{text}");
    let request = serde_json::to_string(&h.provider.requests()[0]).unwrap();
    assert!(request.contains("Extra rule"), "the guideline reaches the system prompt");
    assert!(!request.contains("models."), "models: false leaves the models API out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_captured_codemode_definition_runs_as_a_nested_run_of_the_calling_tool() {
    let source = r#"
import { createCodemodeExtension } from '@earendil-works/pi-coding-agent';
export default function (pi) {
  let definition;
  const shim = new Proxy({ registerTool: (tool) => { definition = tool; } }, { get(target, key) {
    if (key in target) return target[key];
    throw new Error(`codemode read pi.${String(key)}`);
  } });
  createCodemodeExtension()(shim);
  pi.registerTool({
    name: 'run_script', label: 'run_script', description: 'Run a fixed script',
    parameters: { type: 'object', properties: {} },
    execute: (toolCallId, _params, signal, onUpdate, ctx) =>
      definition.execute(toolCallId, { code: 'return await tools.read({ path: "note.txt" });' }, signal, onUpdate, ctx),
  });
}
"#;
    let responses = vec![FauxResponse::tool_call("call-1", "run_script", json!({})), FauxResponse::text("done")];
    let h = start(source, responses, json!({})).await;
    prompt(&h, "run it").await;
    let text = tool_text(&h, "run_script");
    assert!(text.starts_with("Script completed"), "{text}");
    assert!(text.contains("note contents"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_session_factories_and_builtin_entries_register_native_tools() {
    let source = r#"
import { createAgentSession, createCodemodeExtension, createToolSearchExtension, DefaultResourceLoader, getAgentDir,
  SessionManager, SettingsManager } from '@earendil-works/pi-coding-agent';
export default pi => pi.registerCommand('child', { handler: async (_args, ctx) => {
  const settingsManager = SettingsManager.inMemory({ retry: { enabled: false }, compaction: { enabled: false } });
  const loader = new DefaultResourceLoader({
    cwd: ctx.cwd, agentDir: getAgentDir(), settingsManager, noSkills: true, noPromptTemplates: true, noContextFiles: true,
    disabledBuiltinExtensions: ['tool-search'],
    extensionFactories: [
      { name: 'codemode', builtin: true, replaceable: true, factory: createCodemodeExtension() },
      { name: 'tool-search', builtin: true, replaceable: true, factory: createToolSearchExtension() },
      createToolSearchExtension(),
    ],
  });
  await loader.reload();
  const { session } = await createAgentSession({
    cwd: ctx.cwd, model: ctx.model, tools: ['read', 'codemode', 'tool_search'], resourceLoader: loader,
    sessionManager: SessionManager.inMemory(ctx.cwd), settingsManager,
  });
  const tools = session.getAllTools().filter((tool) => tool.name === 'codemode' || tool.name === 'tool_search')
    .map((tool) => [tool.name, tool.sourceInfo.path]).sort();
  const paths = loader.getExtensions().extensions.map((extension) => extension.path);
  await session.prompt('script');
  pi.appendEntry('child', { tools, paths, active: session.getActiveToolNames().sort(), messages: session.messages });
  session.dispose();
}});
"#;
    let responses =
        vec![FauxResponse::tool_call("call-1", "codemode", json!({"code": SCRIPT})), FauxResponse::text("done")];
    let h = start(source, responses, json!({})).await;
    prompt(&h, "/child").await;
    let data = entry(&h, "child");
    assert_eq!(data["paths"], json!(["builtin:codemode", "<inline:1>"]));
    assert_eq!(data["tools"], json!([["codemode", "builtin:codemode"], ["tool_search", "<inline:1>"]]));
    assert_eq!(data["active"], json!(["codemode", "read", "tool_search"]));
    let messages = data["messages"].as_array().unwrap();
    let result = messages.iter().find(|message| message["toolName"] == "codemode").unwrap();
    assert!(result["content"].to_string().contains("note contents"), "{result}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mcp_factory_starts_native_mcp_and_refuses_callback_options() {
    let source = r#"
import { createMcpExtension } from '@earendil-works/pi-coding-agent';
export default function (pi) {
  let refused;
  try { createMcpExtension({ loadConfig: () => ({}) }); } catch (error) { refused = { code: error.code, message: error.message }; }
  createMcpExtension({ startupWaitMs: 0 })(pi);
  pi.on('session_start', () => { pi.appendEntry('refused', refused); });
}
"#;
    let h = start(source, Vec::new(), json!({"extensions": ["-builtin:mcp"]})).await;
    let refused = entry(&h, "refused");
    assert_eq!(refused["code"], "ERR_PI_UNSUPPORTED");
    assert_eq!(refused["message"], "createMcpExtension.loadConfig is not supported in pi-rust");
    assert!(h.session.mcp_manager().is_some(), "the factory started MCP with builtin:mcp off");
    assert!(h.session.builtin_extension_enabled("mcp"));
}
