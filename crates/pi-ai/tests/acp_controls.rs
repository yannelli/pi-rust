//! `acp::open_session`, `acp::close_session`, and the session selectors against `fake-acp-agent`:
//! options known before a prompt, optional categories, legacy agents, and mapped thinking levels.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use pi_ai::providers::acp::{self, AcpHost, AgentLaunch, PermissionRequest, Selector, SessionInfo};
use pi_ai::registry::{ApiProvider, StreamOptions};
use pi_ai::stream::AssistantMessageEvent;
use pi_ai::types::{AssistantMessage, ContentBlock, Context, Message, Model, StopReason, ThinkingLevel};

#[derive(Default)]
struct Host {
    cwd: Mutex<HashMap<String, PathBuf>>,
    sessions: Mutex<Vec<SessionInfo>>,
    turns: Mutex<Vec<String>>,
}

#[async_trait]
impl AcpHost for Host {
    async fn request_permission(&self, _request: PermissionRequest) -> Option<String> {
        None
    }
    fn session_changed(&self, info: &SessionInfo) {
        self.sessions.lock().unwrap().push(info.clone());
    }
    fn session_cwd(&self, pi_session: &str) -> Option<PathBuf> {
        self.cwd.lock().unwrap().get(pi_session).cloned()
    }
    fn live_session(&self, pi_session: &str) -> bool {
        !pi_session.starts_with("summary")
    }
    fn turn_started(&self, pi_session: &str, _cwd: &std::path::Path) {
        self.turns.lock().unwrap().push(pi_session.into());
    }
}

fn pid_file() -> PathBuf {
    std::env::temp_dir().join(format!("fake-acp-pid-{}", std::process::id()))
}

fn launch(provider: &str, variant: &str, extra: &[(&str, String)]) -> AgentLaunch {
    let mut env: BTreeMap<String, String> = [("FAKE_ACP_CONFIG".to_string(), variant.to_string())].into();
    env.extend(extra.iter().map(|(key, value)| (key.to_string(), value.clone())));
    AgentLaunch {
        provider: provider.into(),
        name: format!("Fake {variant}"),
        command: env!("CARGO_BIN_EXE_fake-acp-agent").into(),
        env,
        ..AgentLaunch::default()
    }
}

static HOST: LazyLock<Arc<Host>> = LazyLock::new(|| {
    let host = Arc::new(Host::default());
    acp::set_host(Some(host.clone()));
    let mut agents: Vec<AgentLaunch> = ["default", "numeric", "grouped", "legacy"]
        .into_iter()
        .map(|variant| launch(&format!("fake-{variant}"), variant, &[]))
        .collect();
    agents
        .push(AgentLaunch { command: "/nonexistent/fake-acp-agent".into(), ..launch("fake-missing", "default", &[]) });
    let pid = pid_file().display().to_string();
    agents.push(launch("fake-pid", "default", &[("FAKE_ACP_PID_FILE", pid)]));
    acp::set_agents(agents);
    host
});

fn session(name: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    HOST.cwd.lock().unwrap().insert(name.into(), dir.path().canonicalize().unwrap());
    (dir, name.into())
}

fn model(provider: &str, id: &str, levels: Option<&[(&str, Option<&str>)]>) -> Model {
    let thinking_level_map: Option<BTreeMap<String, Option<String>>> = levels
        .map(|levels| levels.iter().map(|(level, value)| (level.to_string(), value.map(str::to_string))).collect());
    let reasoning = thinking_level_map.is_some();
    Model {
        id: id.into(),
        provider: provider.into(),
        api: acp::API.into(),
        reasoning,
        thinking_level_map,
        ..Model::default()
    }
}

async fn ask(model: &Model, pi_session: &str, level: Option<ThinkingLevel>) -> AssistantMessage {
    let context = Context { messages: vec![Message::user("hi")], ..Context::default() };
    let options = StreamOptions { session_id: Some(pi_session.into()), reasoning: level, ..StreamOptions::default() };
    let mut stream = acp::Acp.stream(model, &context, options);
    let mut last = None;
    while let Some(event) = stream.next().await {
        last = Some(event);
    }
    match last {
        Some(AssistantMessageEvent::Done { message, .. }) => message,
        Some(AssistantMessageEvent::Error { error, .. }) => error,
        other => panic!("the stream did not end with Done or Error: {other:?}"),
    }
}

fn text(message: &AssistantMessage) -> String {
    message.content.iter().filter_map(ContentBlock::as_text).collect()
}

fn selector<'a>(info: &'a SessionInfo, category: &str) -> &'a Selector {
    info.selectors.iter().find(|selector| selector.category == category).expect(category)
}

fn ids(selector: &Selector) -> Vec<&str> {
    selector.choices.iter().map(|choice| choice.id.as_str()).collect()
}

/// Whether process `pid` is gone within three seconds, reaped zombies included.
#[cfg(unix)]
fn exits(pid: &str) -> bool {
    assert!(pid.parse::<u32>().is_ok_and(|pid| pid > 0), "not a process ID: {pid}");
    for _ in 0..60 {
        let alive = std::process::Command::new("kill").args(["-0", pid]).status().unwrap().success();
        let state = std::process::Command::new("ps").args(["-o", "stat=", "-p", pid]).output().unwrap();
        if !alive || String::from_utf8_lossy(&state.stdout).trim_start().starts_with('Z') {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

#[tokio::test]
async fn open_session_knows_the_options_before_a_prompt_and_the_prompt_reuses_it() {
    let (_dir, id) = session("open-default");
    let info = acp::open_session("fake-default", &id).await.unwrap();
    assert_eq!(ids(selector(&info, "model")), ["fast", "smart"]);
    assert_eq!(ids(selector(&info, "thought_level")), ["low", "high"]);
    let mode = selector(&info, "mode");
    assert!(mode.legacy_mode && ids(mode) == ["ask", "code"]);
    assert!(HOST.sessions.lock().unwrap().iter().any(|reported| reported.pi_session == id));
    assert!(!HOST.turns.lock().unwrap().contains(&id), "opening sends no prompt");
    let again = acp::open_session("fake-default", &id).await.unwrap();
    assert_eq!(again.acp_session, info.acp_session);
    let message = ask(&model("fake-default", "smart", None), &id, None).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert!(text(&message).contains("model=smart turn=1 effort=low news=1 "), "{}", text(&message));
}

#[tokio::test]
async fn two_opens_at_once_share_one_session() {
    let (_dir, id) = session("open-twice");
    let (first, second) = tokio::join!(acp::open_session("fake-default", &id), acp::open_session("fake-default", &id));
    assert_eq!(first.unwrap().acp_session, second.unwrap().acp_session);
    assert!(!HOST.turns.lock().unwrap().contains(&id), "opening sends no prompt");
    let message = ask(&model("fake-default", "default", None), &id, None).await;
    assert!(text(&message).contains("turn=1 effort=low news=1 "), "one handshake: {}", text(&message));
}

#[tokio::test]
async fn mode_set_before_open_applies_and_thinking_stays_independent() {
    let (_dir, id) = session("open-mode");
    acp::set_session_option("fake-default", &id, "mode", "code").await.unwrap();
    let info = acp::open_session("fake-default", &id).await.unwrap();
    assert_eq!(selector(&info, "mode").current.as_deref(), Some("code"));
    let levels: &[(&str, Option<&str>)] = &[("off", None), ("low", Some("low")), ("high", Some("high"))];
    let message = ask(&model("fake-default", "default", Some(levels)), &id, Some(ThinkingLevel::High)).await;
    assert!(text(&message).contains("mode=code model=fast turn=1 effort=high "), "{}", text(&message));
}

#[tokio::test]
async fn numeric_effort_without_a_category_takes_the_mapped_value() {
    let (_dir, id) = session("open-numeric");
    let info = acp::open_session("fake-numeric", &id).await.unwrap();
    let effort = selector(&info, "thought_level");
    assert_eq!((effort.id.as_str(), ids(effort)), ("effortLevel", vec!["1", "2", "3"]));
    assert_eq!(selector(&info, "mode").id, "permission", "an uncategorized permission option is the mode");
    assert!(info.selectors.iter().all(|selector| selector.id != "verbose"), "boolean options are not selectors");
    let levels: &[(&str, Option<&str>)] =
        &[("low", Some("1")), ("medium", Some("2")), ("high", Some("3")), ("max", None)];
    let high = ask(&model("fake-numeric", "default", Some(levels)), &id, Some(ThinkingLevel::High)).await;
    assert!(text(&high).contains("effort=3 "), "{}", text(&high));

    let (_other_dir, other) = session("open-numeric-max");
    let max = ask(&model("fake-numeric", "default", Some(levels)), &other, Some(ThinkingLevel::Max)).await;
    assert!(text(&max).contains("effort=2 "), "an unsupported level sends nothing: {}", text(&max));
}

#[tokio::test]
async fn grouped_models_flatten_and_explicit_categories_are_kept() {
    let (_dir, id) = session("open-grouped");
    let info = acp::open_session("fake-grouped", &id).await.unwrap();
    assert_eq!(ids(selector(&info, "model")), ["fast", "smart"]);
    assert_eq!(selector(&info, "_kiro_speed").id, "effort");
    assert!(info.selectors.iter().all(|selector| selector.category != "thought_level"));
    let message = ask(&model("fake-grouped", "default", None), &id, Some(ThinkingLevel::High)).await;
    assert!(text(&message).contains("effort=low "), "{}", text(&message));
}

#[tokio::test]
async fn legacy_agents_offer_models_and_modes_and_take_session_set_model() {
    let (_dir, id) = session("open-legacy");
    let info = acp::open_session("fake-legacy", &id).await.unwrap();
    let models = selector(&info, "model");
    assert!(models.legacy_mode && ids(models) == ["fast", "smart"]);
    assert!(selector(&info, "mode").legacy_mode);
    let message = ask(&model("fake-legacy", "smart", None), &id, None).await;
    assert!(text(&message).contains("model=smart turn=1 "), "{}", text(&message));
}

#[tokio::test]
async fn open_session_refuses_unknown_agents_summaries_and_failed_starts() {
    let (_dir, id) = session("open-errors");
    let unknown = acp::open_session("fake-nobody", &id).await.unwrap_err();
    assert!(unknown.contains("No ACP agent is set up for fake-nobody"), "{unknown}");
    let summary = acp::open_session("fake-default", "summary-open").await.unwrap_err();
    assert!(summary.contains("not a live pi session"), "{summary}");
    assert!(acp::open_session("fake-missing", &id).await.is_err());
    assert!(acp::session_info("fake-missing", &id).is_none(), "a failed start keeps no session");
}

#[cfg(unix)]
#[tokio::test]
async fn close_session_stops_an_opened_agent_and_keeps_other_sessions() {
    let (_dir, other) = session("close-other");
    acp::open_session("fake-default", &other).await.unwrap();
    let (_dir, id) = session("close-opened");
    acp::set_session_option("fake-default", &id, "mode", "code").await.unwrap();
    acp::open_session("fake-pid", &id).await.unwrap();
    let pid = std::fs::read_to_string(pid_file()).unwrap();
    let running = std::process::Command::new("kill").args(["-0", &pid]).status().unwrap().success();
    assert!(running, "the opened agent {pid} runs before close_session");
    acp::close_session(&id).await;
    assert!(exits(&pid), "the agent {pid} still runs after close_session");
    assert!(acp::session_info("fake-pid", &id).is_none());
    assert!(acp::session_info("fake-default", &other).is_some(), "other pi sessions keep their agents");
    let reopened = acp::open_session("fake-default", &id).await.unwrap();
    assert_eq!(selector(&reopened, "mode").current.as_deref(), Some("ask"), "close drops the waiting values");
}
