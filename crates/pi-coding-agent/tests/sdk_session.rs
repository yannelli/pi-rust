#[path = "extensions/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "agent_session/mcp_fake.rs"]
#[allow(dead_code)]
mod mcp_fake;
#[path = "offline_guard/mod.rs"]
mod offline_guard;

use harness::{Setup, harness};
use pi_ai::providers::faux::FauxResponse;
use serde_json::json;

const CHILD: &str = r#"
import { createAgentSession, SessionManager, SettingsManager, DefaultResourceLoader, getAgentDir } from '@earendil-works/pi-coding-agent';
export default function (pi) {
  pi.registerCommand('child', { handler: async (_args, ctx) => {
    const manager = SessionManager.inMemory(ctx.cwd);
    const settings = SettingsManager.inMemory({ retry: { enabled: false }, compaction: { enabled: false } });
    const loader = new DefaultResourceLoader({
      cwd: ctx.cwd, agentDir: getAgentDir(), settingsManager: settings, noExtensions: true, noSkills: true,
      noPromptTemplates: true, noContextFiles: true, systemPromptOverride: () => 'Child prompt',
    });
    await loader.reload();
    const { session } = await createAgentSession({
      cwd: ctx.cwd, model: ctx.model, thinkingLevel: 'low', tools: [],
      resourceLoader: loader, sessionManager: manager, settingsManager: settings,
    });
    const events = [];
    const unsubscribe = session.subscribe(event => events.push(event.type));
    await session.prompt('child question');
    unsubscribe();
    pi.appendEntry('child-result', {
      events, messages: session.messages, model: session.model.id,
      thinkingLevel: session.thinkingLevel, sessionId: session.sessionId,
      managerId: manager.getSessionId(), branch: manager.getBranch(),
      context: manager.buildSessionContext(), streaming: session.isStreaming,
    });
    session.dispose();
    session.dispose();
    pi.appendEntry('after-dispose', { entries: manager.getEntries() });
    try { await session.prompt('disposed'); } catch (error) { pi.appendEntry('disposed-error', { message: error.message }); }
  }});
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_session_streams_events_and_updates_its_manager() {
    let h = harness(Setup {
        files: vec![("child.ts", CHILD.into())],
        load: vec!["child.ts"],
        responses: vec![FauxResponse::text("child answer")],
        ..Setup::default()
    })
    .await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    tokio::time::timeout(std::time::Duration::from_secs(20), h.session.prompt("/child", Vec::new()))
        .await
        .unwrap()
        .unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entries = h.entries();
    let result = &entries.iter().find(|entry| entry["customType"] == "child-result").unwrap()["data"];
    assert_eq!(result["model"], "tiny");
    assert_eq!(result["thinkingLevel"], "low");
    assert_eq!(result["sessionId"], result["managerId"]);
    assert_eq!(result["streaming"], false);
    let events = result["events"].as_array().unwrap();
    for kind in ["agent_start", "message_start", "message_update", "message_end", "agent_end"] {
        assert!(events.contains(&json!(kind)), "{events:?}");
    }
    let messages = result["messages"].as_array().unwrap();
    assert!(
        messages.iter().any(|message| message["role"] == "user" && message["content"][0]["text"] == "child question")
    );
    assert!(
        messages
            .iter()
            .any(|message| message["role"] == "assistant" && message["content"][0]["text"] == "child answer")
    );
    assert_eq!(result["context"]["messages"], result["messages"]);
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 1);
    assert!(serde_json::to_string(&requests[0]).unwrap().contains("Child prompt"));
    assert!(requests[0].tools.is_empty());
    assert!(h.session.messages().is_empty());
    let disposed = &entries.iter().find(|entry| entry["customType"] == "disposed-error").unwrap()["data"];
    assert_eq!(disposed["message"], "AgentSession has been disposed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_session_reports_unsupported_options_and_members() {
    let source = r#"
import { createAgentSession, SessionManager } from '@earendil-works/pi-coding-agent';
export default pi => pi.registerCommand('unsupported-sdk', { handler: async (_args, ctx) => {
  try { await createAgentSession({ customTools: [] }); }
  catch (error) { pi.appendEntry('option-error', { code: error.code, message: error.message }); }
  const { session } = await createAgentSession({ model: ctx.model, tools: [], sessionManager: SessionManager.inMemory(ctx.cwd) });
  try { session.compact(); }
  catch (error) { pi.appendEntry('member-error', { code: error.code, message: error.message }); }
  session.dispose();
}});
"#;
    let h = harness(Setup { files: vec![("sdk.ts", source.into())], load: vec!["sdk.ts"], ..Setup::default() }).await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    h.session.prompt("/unsupported-sdk", Vec::new()).await.unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entries = h.entries();
    for (kind, member) in [("option-error", "createAgentSession.customTools"), ("member-error", "AgentSession.compact")]
    {
        let data = &entries.iter().find(|entry| entry["customType"] == kind).unwrap()["data"];
        assert_eq!(data["code"], "ERR_PI_UNSUPPORTED");
        assert!(data["message"].as_str().unwrap().contains(member));
    }
    assert!(h.provider.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_session_abort_finishes_a_hanging_provider() {
    let source = r#"
import { createAgentSession, SessionManager, SettingsManager } from '@earendil-works/pi-coding-agent';
export default pi => pi.registerCommand('abort-child', { handler: async (_args, ctx) => {
  const { session } = await createAgentSession({
    model: ctx.model, tools: [], sessionManager: SessionManager.inMemory(ctx.cwd),
    settingsManager: SettingsManager.inMemory({ retry: { enabled: false } }),
  });
  let aborted;
  session.subscribe(event => { if (event.type === 'message_update') aborted = session.abort(); });
  const running = session.prompt('hang');
  await new Promise(resolve => setTimeout(resolve, 100));
  await session.abort();
  await running;
  await aborted;
  pi.appendEntry('aborted-child', { streaming: session.isStreaming });
  session.dispose();
}});
"#;
    let h = harness(Setup {
        files: vec![("abort.ts", source.into())],
        load: vec!["abort.ts"],
        responses: vec![FauxResponse::Hang],
        ..Setup::default()
    })
    .await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    tokio::time::timeout(std::time::Duration::from_secs(20), h.session.prompt("/abort-child", Vec::new()))
        .await
        .unwrap()
        .unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    assert!(
        h.entries().iter().any(|entry| entry["customType"] == "aborted-child" && entry["data"]["streaming"] == false)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_session_resumes_a_persisted_transcript() {
    let source = r#"
import { createAgentSession, SessionManager, SettingsManager } from '@earendil-works/pi-coding-agent';
export default pi => pi.registerCommand('file-child', { handler: async (_args, ctx) => {
  const manager = SessionManager.create(ctx.cwd, `${ctx.cwd}/child-sessions`);
  const settingsManager = SettingsManager.inMemory({ retry: { enabled: false }, compaction: { enabled: false } });
  const options = { model: ctx.model, tools: [], settingsManager };
  const first = await createAgentSession({ ...options, sessionManager: manager });
  await first.session.prompt('first question');
  const file = manager.getSessionFile();
  first.session.dispose();
  const reopened = SessionManager.open(file);
  const second = await createAgentSession({ ...options, sessionManager: reopened });
  await second.session.prompt('second question');
  pi.appendEntry('file-child-result', {
    file, id: second.session.sessionId, originalId: manager.getSessionId(),
    messages: second.session.messages,
  });
  second.session.dispose();
}});
"#;
    let h = harness(Setup {
        files: vec![("file.ts", source.into())],
        load: vec!["file.ts"],
        responses: vec![FauxResponse::text("first answer"), FauxResponse::text("second answer")],
        ..Setup::default()
    })
    .await;
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    tokio::time::timeout(std::time::Duration::from_secs(20), h.session.prompt("/file-child", Vec::new()))
        .await
        .unwrap()
        .unwrap();
    assert!(h.errors().is_empty(), "{:?}", h.errors());
    let entries = h.entries();
    let data = &entries.iter().find(|entry| entry["customType"] == "file-child-result").unwrap()["data"];
    assert_eq!(data["id"], data["originalId"]);
    assert!(
        data["messages"].as_array().unwrap().iter().any(|message| message["content"][0]["text"] == "second answer")
    );
    let file = std::path::Path::new(data["file"].as_str().unwrap());
    let manager = pi_coding_agent::session::SessionManager::open(file).unwrap();
    assert_eq!(manager.id(), data["id"].as_str().unwrap());
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(serde_json::to_string(&requests[1]).unwrap().contains("first answer"));
}
