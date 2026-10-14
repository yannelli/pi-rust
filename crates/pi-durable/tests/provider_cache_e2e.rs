//! Port of upstream `test/provider-session-cache-e2e.test.ts` and `test/system-order-cache-e2e.test.ts`. Both
//! call real providers; they are ignored by default and skip when the token is unset, like upstream's `skipIf`.
//! Upstream resolves OAuth tokens; here they come from `PI_CODEX_TOKEN` and `ANTHROPIC_API_KEY`.

mod support;

use std::sync::{Arc, Mutex};

use pi_ai::{
    AssistantMessage, AssistantMessageEventStream, DeferredHandle, Message, Model, StopReason, ThinkingLevel,
    Transport, Usage,
};
use pi_durable::harness::generation::{GenerationHooks, generation_task};
use pi_durable::harness::usage::PROVIDER_DOC;
use pi_durable::harness::*;
use pi_durable::models::{DeferredCancelOptions, DeferredFetchOptions, Models, SimpleStreamOptions, create_models};
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::{ConversationId, DurableResult, EntryId};
use serde_json::json;
use support::context;

/// Delegates to the built-in models with `api_key`, recording the session ID each request carries.
struct Keyed {
    inner: Arc<dyn Models>,
    api_key: String,
    sessions: Arc<Mutex<Vec<String>>>,
}

impl Models for Keyed {
    fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        self.inner.get_models(provider)
    }

    fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.inner.get_model(provider, id)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &pi_ai::Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.sessions.lock().unwrap().push(options.session_id.clone().unwrap_or_default());
        self.inner.stream_simple(model, context, SimpleStreamOptions { api_key: Some(self.api_key.clone()), ..options })
    }

    fn stream_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredFetchOptions,
    ) -> AssistantMessageEventStream {
        self.inner.stream_deferred(model, handle, options)
    }

    fn cancel_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredCancelOptions,
    ) -> futures::future::BoxFuture<'static, DurableResult<()>> {
        self.inner.cancel_deferred(model, handle, options)
    }
}

fn keyed(api_key: String) -> (Arc<dyn Models>, Arc<Mutex<Vec<String>>>) {
    let sessions = Arc::new(Mutex::new(Vec::new()));
    (Arc::new(Keyed { inner: Arc::new(create_models()), api_key, sessions: sessions.clone() }), sessions)
}

fn repeated(header: &[&str], nonce: &str, count: usize, record: &str) -> String {
    let greek = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega.";
    let lines = (0..count).map(|index| format!("{nonce} {record} {index:03}: {greek}"));
    header.iter().map(|line| (*line).to_owned()).chain(lines).collect::<Vec<_>>().join("\n")
}

fn nonce() -> String {
    format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
}

async fn answer(harness: &Harness, conversation_id: ConversationId, answer: EntryId) -> AssistantMessage {
    let entry = harness.commit(move |tx: Tx| async move { tx.entry(answer).await }, &context()).await.unwrap().unwrap();
    assert_eq!(entry.conversation_id, conversation_id, "Answer belongs to another conversation");
    match entry.model.unwrap_or_default().into_iter().next() {
        Some(Message::Assistant(message)) => message,
        _ => panic!("an assistant answer"),
    }
}

async fn ask(harness: &Harness, root: &Conversation, content: String) -> AssistantMessage {
    let settled =
        root.submit(SubmissionDraft::input(content), &context()).await.unwrap().wait(&context()).await.unwrap();
    assert_eq!(settled.status, pi_durable::SubmissionStatus::Done);
    answer(harness, root.id, settled.answer.unwrap()).await
}

fn prompt_tokens(usage: &Usage) -> u64 {
    usage.input + usage.cache_read + usage.cache_write
}

fn hit_rate(usage: &Usage) -> f64 {
    match prompt_tokens(usage) {
        0 => 0.0,
        total => usage.cache_read as f64 / total as f64,
    }
}

fn no_compaction() -> HarnessSettings {
    HarnessSettings {
        compaction: Some(CompactionOverrides { enabled: Some(false), ..Default::default() }),
        ..Default::default()
    }
}

// Needs the OpenAI Codex provider over the network and a token.
#[tokio::test]
#[ignore = "needs a network provider and an OpenAI Codex token"]
async fn reuses_the_codex_prompt_cache_across_durable_turns() {
    let Ok(token) = std::env::var("PI_CODEX_TOKEN") else { return };
    let (models, sessions) = keyed(token);
    let settings = HarnessSettings {
        stream: Some(ConversationStreamOptions { transport: Some(Transport::Sse), ..Default::default() }),
        ..no_compaction()
    };
    let options = HarnessOptions::new(models, Arc::new(create_registry())).with_settings(settings);
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context()).await.unwrap();
    let model = ModelRef { provider: "openai-codex".into(), model_id: "gpt-5.5".into() };
    let root = harness.root(&context(), CreateOptions::default().with_agent(AgentChange::model(model))).await.unwrap();
    harness.resume().unwrap();
    let header = [
        "This is a real automated prompt-cache test. The repeated context below is intentional.",
        "Read it silently, then reply with exactly: CACHE PROBE READY",
    ];
    let (mut usages, mut first_hit) = (Vec::new(), None);
    for turn in 0..8 {
        let content = if turn == 0 {
            repeated(&header, &nonce(), 180, "immutable cache record")
        } else {
            format!("Reply exactly: CACHE PROBE FOLLOW-UP {turn}")
        };
        let message = ask(&harness, &root, content).await;
        assert_ne!(message.stop_reason, StopReason::Error, "{:?}", message.error_message);
        usages.push(message.usage);
        if first_hit.is_none() && usages.last().unwrap().cache_read > 0 {
            first_hit = Some(usages.len() - 1);
        }
        if first_hit.is_some_and(|hit| usages.len() - hit >= 3) {
            break;
        }
    }
    let provider = harness.snapshot(&PROVIDER_DOC, root.id, &context()).await.unwrap().unwrap();
    let session_id = provider.get("sessionId").and_then(pi_chord::Json::as_str).unwrap().to_owned();
    assert_eq!(*sessions.lock().unwrap(), vec![session_id; usages.len()]);
    assert!(prompt_tokens(&usages[0]) >= 4_000);
    assert_eq!((usages[0].cache_read, first_hit), (0, Some(1)));
    let cached = &usages[1..];
    assert_eq!(cached.len(), 3);
    assert!(cached.iter().all(|usage| hit_rate(usage) >= 0.9), "{cached:?}");
    harness.close(&context()).await.unwrap();
}

/// Puts the leading system message back after the user messages before it: the request order before the fix.
fn committed_order(messages: Vec<Message>) -> Vec<Message> {
    let mut rest = messages;
    if !matches!(rest.first(), Some(Message::System(_))) {
        return rest;
    }
    let system = rest.remove(0);
    let at = rest.iter().position(|message| !matches!(message, Message::User(_))).unwrap_or(rest.len());
    rest.insert(at, system);
    rest
}

/// Share of the prompt the second turn read from cache after adding a tool.
async fn cache_read_after_tool_change(token: &str, legacy_order: bool) -> f64 {
    let (models, _) = keyed(token.to_owned());
    let registry = Arc::new(create_registry());
    let parameters = json!({ "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] });
    let unused = |name: &str, description: &str| {
        define_tool(name, description, parameters.clone(), |_, _, _| async {
            Ok(ToolExecutionResult { output: Some(Vec::new()), ..Default::default() })
        })
    };
    let probe = Extension::new("probe")
        .tool(unused("probe_first", "Unused probe tool."))
        .tool(unused("probe_second", "Unused probe tool added on the second turn."));
    let reorder = GenerationHooks {
        before_request: Some(Arc::new(|messages, _, _| Box::pin(async move { Ok(Some(committed_order(messages))) }))),
        ..GenerationHooks::default()
    };
    registry.install(probe).unwrap();
    registry.install(Extension::new("legacy-order").hook(hook(&generation_task(), reorder))).unwrap();
    let options = HarnessOptions::new(models, registry).with_settings(no_compaction());
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context()).await.unwrap();
    let mut extensions = vec!["probe".to_owned()];
    if legacy_order {
        extensions.push("legacy-order".into());
    }
    let header = [
        "This is an automated prompt-cache test. The repeated context below is intentional.",
        "Never call tools. Reply with exactly the text the user asks for.",
    ];
    let agent = AgentChange {
        thinking_level: Change::Set(ThinkingLevel::Low),
        instructions: Change::Set(repeated(&header, &nonce(), 250, "cache record")),
        extensions: Change::Set(ExtensionSelection::List(extensions)),
        tools: Change::Set(ToolFilter::List(vec!["probe_first".into()])),
        ..AgentChange::model(ModelRef { provider: "anthropic".into(), model_id: "claude-sonnet-5-5".into() })
    };
    let root = harness.root(&context(), CreateOptions::default().with_agent(agent)).await.unwrap();
    harness.resume().unwrap();
    let before = ask(&harness, &root, "Reply exactly: READY".into()).await;
    assert_eq!(before.stop_reason, StopReason::Stop, "{:?}", before.error_message);
    assert!(prompt_tokens(&before.usage) >= 4_000);
    let both = ToolFilter::List(vec!["probe_first".into(), "probe_second".into()]);
    root.configure(AgentChange { tools: Change::Set(both), ..Default::default() }, &context()).await.unwrap();
    let after = ask(&harness, &root, "Reply exactly: READY AGAIN".into()).await;
    assert_eq!(after.stop_reason, StopReason::Stop, "{:?}", after.error_message);
    harness.close(&context()).await.unwrap();
    hit_rate(&after.usage)
}

// Needs the Anthropic provider over the network and an API key.
#[tokio::test]
#[ignore = "needs a network provider and an Anthropic API key"]
async fn keeps_the_prompt_cache_across_a_tool_change_only_when_the_system_message_leads() {
    let Ok(token) = std::env::var("ANTHROPIC_API_KEY") else { return };
    assert!(cache_read_after_tool_change(&token, true).await < 0.2);
    assert!(cache_read_after_tool_change(&token, false).await > 0.8);
}
