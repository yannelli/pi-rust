//! Preparation, request options, and settings cases of upstream `test/harness-generation.test.ts`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};

use pi_ai::{ClientAttribution, ClientIdentity, Message, ThinkingLevel, UserAgentMode, UserContent, pi_attribution};
use pi_chord::Json;
use pi_durable::documents::{DocSpec, DocToken, LatestConversation, LatestFork, define_doc};
use pi_durable::harness::usage::PROVIDER_DOC;
use pi_durable::harness::*;
use pi_durable::models::SimpleStreamOptions;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::FauxResponseStep;
use pi_durable::*;
use serde_json::json;

use super::support::harness_support::*;
use super::{answer, error_503};

async fn session_id(harness: &Harness, conversation: &Conversation) -> Option<String> {
    let provider = harness.snapshot(&PROVIDER_DOC, conversation.id, &ctx()).await.unwrap()?;
    provider["sessionId"].as_str().map(str::to_owned)
}

async fn ask(conversation: &Conversation, text: &str) -> SubmissionRecord {
    conversation.submit(SubmissionDraft::input(text), &ctx()).await.unwrap().wait(&ctx()).await.unwrap()
}

#[tokio::test]
async fn reports_section_wrapper_failures_while_preparing() {
    let setup = chat_setup(Default::default());
    add_section(&setup.registry, "cwd", "/repo", None);
    let broken = wrap_section("cwd", |_| Err(DurableError::error("wrapper failed")));
    setup.registry.install(Extension::new("broken").wrap(broken)).unwrap();
    setup.faux.set_responses([answer("ok")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    harness.resume().unwrap();
    assert_eq!(ask(&root, "hi").await.status, SubmissionStatus::Done);
    assert!(setup.reports().contains(&"wrapper failed".to_owned()), "{:?}", setup.reports());
    // The failed section is absent, so nothing was rendered.
    assert_eq!(kinds(&all_entries(&root).await), ["pi.user", "pi.assistant"]);
    harness.close(&ctx()).await.unwrap();
}

/// What a request saw of its options.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    timeout_ms: Option<u64>,
    headers: BTreeMap<String, String>,
    reasoning: Option<ThinkingLevel>,
    session_id: Option<String>,
    signal: bool,
}

fn seen(options: &SimpleStreamOptions) -> Seen {
    Seen {
        timeout_ms: options.timeout_ms,
        headers: options.headers.clone(),
        reasoning: options.reasoning,
        session_id: options.session_id.clone(),
        signal: options.signal.is_some(),
    }
}

#[tokio::test]
async fn forwards_stream_options_and_the_thinking_level() {
    let setup = chat_setup(Default::default());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let capture = |text: &'static str| {
        let sink = requests.clone();
        FauxResponseStep::factory(move |_, options, _, _| {
            sink.lock().unwrap().push(seen(options));
            answer(text)
        })
    };
    setup.faux.set_responses([capture("a"), capture("b")]);
    let headers = BTreeMap::from([("x-test".to_owned(), "1".to_owned())]);
    let stream =
        ConversationStreamOptions { timeout_ms: Some(1234), headers: Some(headers.clone()), ..Default::default() };
    setup.settings.lock().unwrap().stream = Some(stream);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let high = AgentChange { thinking_level: Change::Set(ThinkingLevel::High), ..AgentChange::default() };
    root.configure(high, &ctx()).await.unwrap();
    harness.resume().unwrap();
    ask(&root, "one").await;
    // Both are read at the next preparation: the thinking level from pi.agent, the stream options live from settings.
    root.configure(AgentChange { thinking_level: Change::Clear, ..AgentChange::default() }, &ctx()).await.unwrap();
    setup.settings.lock().unwrap().stream =
        Some(ConversationStreamOptions { timeout_ms: Some(99), ..Default::default() });
    ask(&root, "two").await;
    let session = session_id(&harness, &root).await;
    let requests = requests.lock().unwrap().clone();
    let first = Seen {
        timeout_ms: Some(1234),
        headers,
        reasoning: Some(ThinkingLevel::High),
        session_id: session.clone(),
        signal: true,
    };
    assert_eq!(requests[0], first);
    let second =
        Seen { timeout_ms: Some(99), headers: BTreeMap::new(), reasoning: None, session_id: session, signal: true };
    assert_eq!(requests[1], second);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn attributes_model_requests_to_pi_by_default_and_merges_the_harness_identity_over_it() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let acme = ClientAttribution { name: "acme".into(), url: "https://acme.example".into(), categories: None };
    let short = ClientIdentity { user_agent: Some(UserAgentMode::Short), ..ClientIdentity::attributed(acme) };
    for identity in [None, Some(short.clone())] {
        let setup = chat_setup(Default::default());
        let sink = seen.clone();
        setup.faux.set_responses([FauxResponseStep::factory(move |_, options, _, _| {
            sink.lock().unwrap().push(options.identity.clone());
            answer("ok")
        })]);
        let options = setup.options();
        let options = match identity {
            Some(identity) => options.with_identity(identity),
            None => options,
        };
        let (harness, root) = open_chat_with(Arc::new(MemoryStorage::new()), &setup, options).await;
        harness.resume().unwrap();
        ask(&root, "hi").await;
        harness.close(&ctx()).await.unwrap();
    }
    assert_eq!(*seen.lock().unwrap(), [Some(ClientIdentity::attributed(pi_attribution())), Some(short)]);
}

/// The text of the last user message of a request.
fn last_user_text(request: &pi_ai::Context) -> String {
    let user = request.messages.iter().rev().find_map(|message| match message {
        Message::User(user) => Some(user),
        _ => None,
    });
    match user.map(|user| &user.content) {
        Some(UserContent::Text(text)) => text.clone(),
        _ => String::new(),
    }
}

// Regression coverage for upstream #10424.
#[tokio::test]
async fn keeps_provider_session_ids_request_local_across_concurrent_conversations() {
    let setup = chat_setup(Default::default());
    let seen = Arc::new(Mutex::new(HashMap::<String, Vec<String>>::new()));
    let capture = || {
        let sink = seen.clone();
        FauxResponseStep::factory(move |request, options, _, _| {
            let text = last_user_text(request);
            let id = options.session_id.clone().unwrap_or_default();
            sink.lock().unwrap().entry(text.clone()).or_default().push(id);
            answer(&format!("answer:{text}"))
        })
    };
    setup.faux.set_responses([capture(), capture(), capture(), capture()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let options = CreateOptions {
        ownership: ConversationOwnership::Ownerless,
        ..CreateOptions::default().with_agent(AgentChange::model(faux_model()))
    };
    let child = harness.create_conversation(options, &ctx()).await.unwrap();
    harness.resume().unwrap();
    for round in ["first", "second"] {
        let (root_text, child_text) = (format!("{round}-0"), format!("{round}-1"));
        futures::join!(ask(&root, &root_text), ask(&child, &child_text));
    }
    let (root_id, child_id) = (session_id(&harness, &root).await.unwrap(), session_id(&harness, &child).await.unwrap());
    assert_ne!(root_id, child_id);
    let seen = seen.lock().unwrap().clone();
    for (text, id) in [("first-0", &root_id), ("second-0", &root_id), ("first-1", &child_id), ("second-1", &child_id)] {
        assert_eq!(seen[text], std::slice::from_ref(id), "{text}");
    }
    harness.close(&ctx()).await.unwrap();
}

fn is_uuid_v7(id: &str) -> bool {
    let parts: Vec<&str> = id.split('-').collect();
    let lengths: Vec<usize> = parts.iter().map(|part| part.len()).collect();
    let hex = id.chars().all(|char| char == '-' || char.is_ascii_digit() || ('a'..='f').contains(&char));
    hex && lengths == [8, 4, 4, 4, 12] && parts[2].starts_with('7') && parts[3].starts_with(['8', '9', 'a', 'b'])
}

// Regression coverage for upstream #10424.
#[tokio::test]
async fn creates_and_persists_provider_state_before_a_legacy_conversation_s_request() {
    let setup = chat_setup(Default::default());
    let sent = Arc::new(Mutex::new(None));
    let sink = sent.clone();
    setup.faux.set_responses([FauxResponseStep::factory(move |_, options, _, _| {
        *sink.lock().unwrap() = options.session_id.clone();
        answer("ok")
    })]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let id = root.id;
    root.commit(move |tx: Tx| async move { tx.retire_doc(&PROVIDER_DOC, id).await }, &ctx()).await.unwrap();
    assert_eq!(session_id(&harness, &root).await, None);
    harness.resume().unwrap();
    ask(&root, "legacy").await;
    let stored = session_id(&harness, &root).await.unwrap();
    assert!(is_uuid_v7(&stored), "{stored}");
    assert_eq!(*sent.lock().unwrap(), Some(stored));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reads_settings_through_getters_at_every_decision() {
    let setup = chat_setup(Default::default());
    let timeouts = Arc::new(Mutex::new(Vec::new()));
    let (sink, settings) = (timeouts.clone(), setup.settings.clone());
    let first = FauxResponseStep::factory(move |_, options, _, _| {
        sink.lock().unwrap().push(options.timeout_ms);
        // The user changes the setting while the first try runs.
        settings.lock().unwrap().stream =
            Some(ConversationStreamOptions { timeout_ms: Some(222), ..Default::default() });
        error_503()
    });
    let sink = timeouts.clone();
    let second = FauxResponseStep::factory(move |_, options, _, _| {
        sink.lock().unwrap().push(options.timeout_ms);
        answer("ok")
    });
    setup.faux.set_responses([first, second]);
    {
        let mut settings = setup.settings.lock().unwrap();
        settings.stream = Some(ConversationStreamOptions { timeout_ms: Some(111), ..Default::default() });
        settings.retry = Some(RetryOverrides { base_delay_ms: Some(1), ..Default::default() });
    }
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    harness.resume().unwrap();
    assert_eq!(ask(&root, "hi").await.status, SubmissionStatus::Done);
    // The retry prepares again, so it resolves the settings again and sends the new timeout.
    assert_eq!(*timeouts.lock().unwrap(), [Some(111), Some(222)]);
    harness.close(&ctx()).await.unwrap();
}

#[test]
fn resolves_settings_over_the_built_in_defaults() {
    let settings = resolve_settings(None);
    assert_eq!(serde_json::to_value(&settings.stream).unwrap(), json!({}));
    let retry = json!({ "enabled": true, "maxRetries": 3, "baseDelayMs": 2000, "maxAgentDelayMs": 60000 });
    assert_eq!(serde_json::to_value(settings.retry).unwrap(), retry);
    let compaction =
        json!({ "enabled": true, "reserveTokens": 16384, "keepRecentTokens": 20000, "backgroundTokens": 32768 });
    assert_eq!(serde_json::to_value(settings.compaction).unwrap(), compaction);
    let progress = json!({ "partialIntervalMs": 100, "outputIntervalMs": 100 });
    assert_eq!(serde_json::to_value(settings.progress).unwrap(), progress);
    assert_eq!(settings.tool_execution, ToolExecutionMode::Parallel);
    assert_eq!((settings.steering_mode, settings.follow_up_mode), (QueueMode::OneAtATime, QueueMode::OneAtATime));
    assert_eq!(settings.context_retention_ms, 600_000.0);
    assert_eq!((settings.after_failed_run, settings.restart_limit), (AfterFailedRun::Keep, 5.0));
    let limit = |restart_limit: f64| {
        resolve_settings(Some(&HarnessSettings { restart_limit: Some(restart_limit), ..HarnessSettings::default() }))
            .restart_limit
    };
    assert_eq!((limit(0.0), limit(2.7), limit(f64::INFINITY)), (5.0, 2.0, f64::INFINITY));

    let overrides = HarnessSettings {
        retry: Some(RetryOverrides { enabled: Some(false), ..Default::default() }),
        compaction: Some(CompactionOverrides { background_tokens: Some(0), ..Default::default() }),
        ..HarnessSettings::default()
    };
    let resolved = resolve_settings(Some(&overrides));
    assert_eq!((resolved.retry.enabled, resolved.retry.max_retries, resolved.retry.base_delay_ms), (false, 3, 2000));
    assert_eq!((resolved.compaction.enabled, resolved.compaction.background_tokens), (true, 0));
    let output = HarnessSettings {
        progress: Some(ProgressOverrides { output_interval_ms: Some(500), ..Default::default() }),
        ..HarnessSettings::default()
    };
    let progress = serde_json::to_value(resolve_settings(Some(&output)).progress).unwrap();
    assert_eq!(progress, json!({ "partialIntervalMs": 100, "outputIntervalMs": 500 }));
}

static TEST_AGENT: LazyLock<DocToken<Json, LatestConversation>> = LazyLock::new(|| {
    let initial = || Json::from(json!({ "cwd": "/", "kind": "main" }));
    define_doc(DocSpec::new("test.agent", 1, LatestConversation(LatestFork::Current), initial, |_, _, _| Ok(false)))
        .unwrap()
});

async fn agent_field(input: &PromptInput, context: &pi_chord::Context, field: &str) -> DurableResult<Option<String>> {
    let agent = input.read.snapshot(&TEST_AGENT, input.conversation_id, context).await?;
    Ok(agent.and_then(|agent| agent[field].as_str().map(str::to_owned)))
}

fn install_section(registry: &Registry, section: std::sync::Arc<PromptSection>) {
    registry.install(Extension::new(format!("section:{}", section.key)).section(section)).unwrap();
}

#[tokio::test]
async fn renders_sections_that_read_conversation_documents_through_input_read() {
    let setup = chat_setup(Default::default());
    let cwd = section("cwd", |input, context| async move { agent_field(&input, &context, "cwd").await }, None);
    install_section(&setup.registry, cwd);
    let agents = section(
        "agents",
        |input, context| async move {
            let kind = agent_field(&input, &context, "kind").await?;
            Ok(if kind.as_deref() == Some("sub") { None } else { Some("Read AGENTS.md".into()) })
        },
        None,
    );
    install_section(&setup.registry, agents);
    setup.faux.set_responses([answer("a"), answer("b")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let id = root.id;
    let set_cwd = move |tx: Tx| async move {
        tx.doc(&TEST_AGENT, id).await?.set(&Json::from(json!({ "cwd": "/repo", "kind": "main" })))
    };
    root.commit(set_cwd, &ctx()).await.unwrap();
    let init: ConversationInit = Box::new(|tx: Tx, id| {
        Box::pin(
            async move { tx.doc(&TEST_AGENT, id).await?.set(&Json::from(json!({ "cwd": "/sub", "kind": "sub" }))) },
        )
    });
    let options = CreateOptions {
        ownership: ConversationOwnership::Ownerless,
        init: Some(init),
        ..CreateOptions::default().with_agent(AgentChange::model(faux_model()))
    };
    let sub = harness.create_conversation(options, &ctx()).await.unwrap();
    harness.resume().unwrap();
    ask(&root, "one").await;
    ask(&sub, "two").await;
    let sections = |entries: Vec<EntryRecord>| {
        let system = entries.into_iter().find(|entry| entry.kind == "pi.system").unwrap();
        let Some(Message::System(system)) = system.model.unwrap().into_iter().next() else { panic!("no system") };
        serde_json::to_value(&system.sections).unwrap()
    };
    let expected = json!({ "cwd": "<cwd>\n/repo\n</cwd>", "agents": "<agents>\nRead AGENTS.md\n</agents>" });
    assert_eq!(sections(all_entries(&root).await), expected);
    assert_eq!(sections(all_entries(&sub).await), json!({ "cwd": "<cwd>\n/sub\n</cwd>" }));
    harness.close(&ctx()).await.unwrap();
}
