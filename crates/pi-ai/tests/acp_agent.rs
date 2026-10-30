//! The `acp` provider against `fake-acp-agent` over real stdio pipes. Each test uses its own pi
//! session ID; the shared host answers by that ID.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use pi_ai::providers::acp::{self, AcpHost, AgentLaunch, PermissionRequest, PlanEntry, SessionInfo};
use pi_ai::registry::{ApiProvider, StreamOptions};
use pi_ai::stream::AssistantMessageEvent;
use pi_ai::types::{AssistantMessage, ContentBlock, Context, Message, Model, StopReason};
use tokio_util::sync::CancellationToken;

const PROVIDER: &str = "fake-acp";

#[derive(Default)]
struct Host {
    cwd: Mutex<HashMap<String, PathBuf>>,
    permissions: Mutex<Vec<PermissionRequest>>,
    plans: Mutex<Vec<Vec<PlanEntry>>>,
    sessions: Mutex<Vec<SessionInfo>>,
}

#[async_trait]
impl AcpHost for Host {
    async fn request_permission(&self, request: PermissionRequest) -> Option<String> {
        let answer = if request.pi_session.contains("reject") { "no" } else { "yes" };
        self.permissions.lock().unwrap().push(request);
        Some(answer.into())
    }
    fn session_changed(&self, info: &SessionInfo) {
        self.sessions.lock().unwrap().push(info.clone());
    }
    fn recall_session(&self, _provider: &str, pi_session: &str) -> Option<String> {
        pi_session.contains("resume").then(|| "s1".into())
    }
    fn plan(&self, _pi_session: &str, entries: &[PlanEntry]) {
        self.plans.lock().unwrap().push(entries.to_vec());
    }
    fn session_cwd(&self, pi_session: &str) -> Option<PathBuf> {
        self.cwd.lock().unwrap().get(pi_session).cloned()
    }
}

static HOST: LazyLock<Arc<Host>> = LazyLock::new(|| {
    let host = Arc::new(Host::default());
    acp::set_host(Some(host.clone()));
    acp::set_agents(vec![AgentLaunch {
        provider: PROVIDER.into(),
        name: "Fake agent".into(),
        command: env!("CARGO_BIN_EXE_fake-acp-agent").into(),
        ..AgentLaunch::default()
    }]);
    host
});

fn model(id: &str) -> Model {
    Model { id: id.into(), provider: PROVIDER.into(), api: acp::API.into(), ..Model::default() }
}

fn session(name: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    HOST.cwd.lock().unwrap().insert(name.into(), dir.path().canonicalize().unwrap());
    (dir, name.into())
}

fn options(session: &str, cancel: CancellationToken) -> StreamOptions {
    StreamOptions { session_id: Some(session.into()), cancel, ..StreamOptions::default() }
}

async fn ask(
    model_id: &str,
    messages: Vec<Message>,
    options: StreamOptions,
) -> (Vec<AssistantMessageEvent>, AssistantMessage) {
    let context = Context { messages, ..Context::default() };
    let mut stream = acp::Acp.stream(&model(model_id), &context, options);
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    let last = match events.last() {
        Some(AssistantMessageEvent::Done { message, .. }) => message.clone(),
        Some(AssistantMessageEvent::Error { error, .. }) => error.clone(),
        other => panic!("the stream did not end with Done or Error: {other:?}"),
    };
    (events, last)
}

fn text(message: &AssistantMessage) -> String {
    message.content.iter().filter_map(ContentBlock::as_text).collect()
}

#[tokio::test]
async fn streams_tool_calls_with_permission_fs_and_terminal() {
    let (dir, id) = session("tools");
    std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
    let (_, message) = ask("default", vec![Message::user("tools")], options(&id, CancellationToken::new())).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let text = text(&message);
    assert!(text.contains("Hello [text] mode=ask model=fast"), "{text}");
    assert!(text.contains("> `edit` Edit a.txt\n") && text.contains("Edit a.txt: **completed**"), "{text}");
    assert!(text.contains("```diff\n") && text.contains("-old\n+new"), "{text}");
    assert!(text.contains("hi from terminal") && text.contains("- [x] Edit a.txt"), "{text}");
    assert!(text.contains("exit=0 outside-rejected=true"), "{text}");
    assert!(
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Thinking { thinking, .. } if thinking == "thinking"))
    );
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "new\n");
    assert_eq!((message.usage.output, message.usage.total_tokens), (5, 1234));
    assert!((message.usage.cost.total - 0.5).abs() < 1e-9);
    let permissions = HOST.permissions.lock().unwrap();
    let request = permissions.iter().find(|request| request.pi_session == id).unwrap();
    assert_eq!(request.title, "Edit a.txt");
    assert_eq!(
        request.options.iter().map(|o| o.2.as_str()).collect::<Vec<_>>(),
        ["allow_once", "allow_always", "reject_once"]
    );
    assert!(HOST.plans.lock().unwrap().iter().any(|plan| plan[0].status == "completed"));
}

#[tokio::test]
async fn a_rejected_permission_fails_the_tool_call() {
    let (dir, id) = session("tools-reject");
    std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
    let (_, message) = ask("default", vec![Message::user("tools")], options(&id, CancellationToken::new())).await;
    assert!(text(&message).contains("Edit a.txt: **failed**"), "{}", text(&message));
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "old\n");
}

#[tokio::test]
async fn cancel_ends_the_turn_as_aborted() {
    let (_dir, id) = session("cancel");
    let cancel = CancellationToken::new();
    let context = Context { messages: vec![Message::user("wait")], ..Context::default() };
    let mut stream = acp::Acp.stream(&model("default"), &context, options(&id, cancel.clone()));
    let mut last = None;
    while let Some(event) = stream.next().await {
        if matches!(event, AssistantMessageEvent::TextDelta { .. }) {
            cancel.cancel();
        }
        last = Some(event);
    }
    match last {
        Some(AssistantMessageEvent::Error { reason, .. }) => assert_eq!(reason, StopReason::Aborted),
        other => panic!("expected an aborted error, got {other:?}"),
    }
}

#[tokio::test]
async fn modes_and_models_are_selectable() {
    let (_dir, id) = session("modes");
    acp::set_session_option(PROVIDER, &id, "mode", "code").await.unwrap();
    let (_, message) = ask("smart", vec![Message::user("hi")], options(&id, CancellationToken::new())).await;
    assert!(text(&message).contains("mode=code model=smart"), "{}", text(&message));
    let info = acp::session_info(PROVIDER, &id).unwrap();
    let models: Vec<String> = info.models().into_iter().map(|choice| choice.id).collect();
    assert_eq!(models, ["fast", "smart"]);
    let mode = info.selectors.iter().find(|selector| selector.category == "mode").unwrap();
    assert_eq!((mode.current.as_deref(), mode.choices.len()), (Some("code"), 2));
    let error = ask("huge", vec![Message::user("hi")], options(&id, CancellationToken::new())).await.1;
    assert!(error.error_message.unwrap().contains("it offers: fast, smart"));
    assert!(HOST.sessions.lock().unwrap().iter().any(|session| session.pi_session == id));
}

#[tokio::test]
async fn resumed_sessions_skip_the_replay_and_send_only_the_new_request() {
    let (_dir, id) = session("resume");
    let earlier =
        AssistantMessage { content: vec![ContentBlock::text("before")], api: acp::API.into(), ..Default::default() };
    let messages = vec![Message::user("first"), Message::Assistant(earlier), Message::user("again @nothing.md")];
    let (_, message) = ask("default", messages, options(&id, CancellationToken::new())).await;
    let text = text(&message);
    assert!(text.starts_with("Hello [text] ") && !text.contains("replayed"), "{text}");
}

#[tokio::test]
async fn a_new_session_gets_the_earlier_conversation_and_file_links() {
    let (dir, id) = session("history");
    std::fs::write(dir.path().join("notes.md"), "x").unwrap();
    let messages = vec![Message::user("first"), Message::user("read @notes.md")];
    let (_, message) = ask("default", messages, options(&id, CancellationToken::new())).await;
    assert!(text(&message).contains("Hello [resource,text,resource_link]"), "{}", text(&message));
}

#[tokio::test]
async fn auth_methods_include_terminal_logins() {
    let agent = {
        LazyLock::force(&HOST);
        acp::agent(PROVIDER).unwrap()
    };
    let cwd = std::env::temp_dir();
    let methods = acp::auth_methods(&agent, &cwd).await.unwrap();
    assert_eq!(methods.len(), 2);
    assert!(methods[0].terminal.is_none());
    let terminal = methods[1].terminal.clone().unwrap();
    assert_eq!(
        (terminal.command.as_str(), terminal.args.as_slice()),
        (agent.command.as_str(), &["--login".to_string()][..])
    );
    acp::authenticate(&agent, &cwd, Some("agent-login")).await.unwrap();
}
