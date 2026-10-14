//! Examples 06 to 12: the Harness, agent configuration, typed entries, context views, registry reloads,
//! extension state, and tasks.

use std::collections::BTreeMap;
use std::sync::Mutex;

use pi_ai::{Message, ThinkingLevel, UserContent, UserMessage};
use pi_durable::entries::{TypedEntryDraft, define_entry};
use pi_durable::harness::agent::AGENT_DOC;
use pi_durable::models::create_models;
use pi_durable::tasks::TaskOptions;
use pi_durable::testing::faux::{FauxMessageOptions, faux_assistant_message, faux_tool_call};

use super::*;

fn user(text: &str) -> Message {
    Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp: 1 })
}

async fn open(registry: &Arc<Registry>) -> Harness {
    let options = HarnessOptions::new(Arc::new(create_models()), registry.clone() as Arc<dyn RegistryReader>);
    Harness::open(memory(), options, &ctx()).await.unwrap()
}

fn agent_doc(value: Option<Json>) -> Value {
    value.map_or(Value::Null, |value| value.to_serde())
}

#[tokio::test]
async fn ex06_harness_resolves_the_root_agent_from_the_registry() {
    let registry = Arc::new(create_registry());
    registry.install(Extension::new("files").tool(path_tool("read", "Read a file"))).unwrap();
    let harness = open(&registry).await;
    let agent = AgentChange { thinking_level: Change::Set(ThinkingLevel::Low), ..AgentChange::default() };
    let init = |tx: Tx, root: ConversationId| -> futures::future::BoxFuture<'static, DurableResult<()>> {
        Box::pin(async move { tx.doc(&NOTES, root).await?.edit(|notes| notes.text = "root notes".into()) })
    };
    let root = harness.root(&ctx(), CreateOptions::default().with_agent(agent).with_init(init)).await.unwrap();
    assert_eq!(notes(harness.snapshot(&NOTES, root.id, &ctx()).await.unwrap()).as_deref(), Some("root notes"));
    let stored = agent_doc(harness.snapshot(&AGENT_DOC, root.id, &ctx()).await.unwrap());
    assert_eq!(stored, json!({ "thinkingLevel": "low" }));
    let agent = root.agent(&ctx()).await.unwrap();
    let extensions: Vec<&str> = agent.extensions.iter().map(|extension| extension.name.as_str()).collect();
    assert_eq!(
        (agent.thinking_level, extensions, tool_names(&agent)),
        (ThinkingLevel::Low, vec!["files"], vec!["read".into()])
    );
    harness.close(&ctx()).await.unwrap();
}

/// Upstream extends the tool type with a `snippet` field; Rust tools have a fixed type, so the snippets live here.
fn snippet(tool: &str) -> String {
    format!("Use {tool} for files.")
}

#[tokio::test]
async fn ex07_configuration_changes_model_thinking_tools_and_extensions() {
    let registry = Arc::new(create_registry());
    let files = Extension::new("files").tool(path_tool("read", "Read a file")).tool(path_tool("write", "Write a file"));
    registry.install(files.clone()).unwrap();
    registry.install(Extension::new("search").tool(path_tool("grep", "Search files"))).unwrap();
    let snippets = section(
        "tool_snippets",
        |input: PromptInput, _| {
            std::future::ready(Ok(Some(
                input.agent.tools.iter().map(|t| snippet(t.name())).collect::<Vec<_>>().join("\n"),
            )))
        },
        None,
    );
    registry.install(Extension::new("snippets").section(snippets)).unwrap();
    // Settings are read through a source at each use, like the upstream getter.
    let timeout = Arc::new(Mutex::new(60_000u64));
    let source = timeout.clone();
    let mut options = HarnessOptions::new(Arc::new(create_models()), registry.clone() as Arc<dyn RegistryReader>);
    options.settings = Some(Arc::new(move || {
        let stream = ConversationStreamOptions { timeout_ms: Some(*source.lock().unwrap()), ..Default::default() };
        let retry = RetryOverrides { max_retries: Some(5), ..Default::default() };
        let tool_execution = Some(ToolExecutionMode::Sequential);
        Ok(HarnessSettings { stream: Some(stream), retry: Some(retry), tool_execution, ..Default::default() })
    }));
    let harness = Harness::open(memory(), options, &ctx()).await.unwrap();
    let root = harness.root(&ctx(), CreateOptions::default()).await.unwrap();
    let tools = || async { tool_names(&root.agent(&ctx()).await.unwrap()) };
    assert_eq!(tools().await, ["read", "write", "grep"]);

    let model = ModelRef { provider: "anthropic".into(), model_id: "claude-sonnet-4-5".into() };
    let change = AgentChange {
        thinking_level: Change::Set(ThinkingLevel::High),
        tools: Change::Set(ToolFilter::List(vec!["write".into(), "read".into()])),
        ..AgentChange::model(model.clone())
    };
    root.configure(change, &ctx()).await.unwrap();
    let stored = agent_doc(harness.snapshot(&AGENT_DOC, root.id, &ctx()).await.unwrap());
    let expected = json!({
        "model": { "provider": "anthropic", "modelId": "claude-sonnet-4-5" }, "thinkingLevel": "high",
        "tools": ["write", "read"],
    });
    assert_eq!(stored, expected);
    let agent = root.agent(&ctx()).await.unwrap();
    assert_eq!((agent.model, agent.thinking_level), (Some(model), ThinkingLevel::High));
    // A tool list sets both the offered tools and their order.
    assert_eq!(tools().await, ["write", "read"]);

    let remove = ExtensionSelection::Edit { add: None, remove: Some(vec!["search".into()]) };
    root.configure(AgentChange { extensions: Change::Set(remove), tools: Change::Clear, ..Default::default() }, &ctx())
        .await
        .unwrap();
    assert_eq!(tools().await, ["read", "write"]);
    let both = ExtensionSelection::List(vec!["files".into(), "search".into()]);
    root.configure(AgentChange { extensions: Change::Set(both), ..Default::default() }, &ctx()).await.unwrap();
    registry.uninstall("files").unwrap();
    assert_eq!(tools().await, ["grep"]);
    registry.install(files).unwrap();
    assert_eq!(tools().await, ["read", "write", "grep"]);
    *timeout.lock().unwrap() = 120_000; // The next request reads the new value.
    harness.close(&ctx()).await.unwrap();
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct From {
    from: String,
}

#[tokio::test]
async fn ex08_harness_conversations_appends_typed_entries_and_inherits_agents_in_forks() {
    let harness = open(&Arc::new(create_registry())).await;
    let root = harness.root(&ctx(), CreateOptions::default()).await.unwrap();
    let high = AgentChange { thinking_level: Change::Set(ThinkingLevel::High), ..AgentChange::default() };
    root.configure(high, &ctx()).await.unwrap();
    let message = define_entry::<From>("message").unwrap();
    let draft =
        TypedEntryDraft { model: Some(vec![user("hello")]), ..TypedEntryDraft::new(From { from: "example".into() }) };
    let (id, draft) = (root.id, message.draft(draft).unwrap());
    let hello = root.commit(move |tx: Tx| async move { tx.append_entry(id, draft).await }, &ctx()).await.unwrap();
    assert!(message.is(Some(&hello)));
    assert_eq!(message.decode(hello.clone()).unwrap().unwrap().data.from, "example");

    let minimal = AgentChange { thinking_level: Change::Set(ThinkingLevel::Minimal), ..AgentChange::default() };
    let helper = harness.create_conversation(CreateOptions::default().with_agent(minimal), &ctx()).await.unwrap();
    let retry = root.fork(hello.id, CreateOptions::default(), &ctx()).await.unwrap();
    assert_eq!(helper.agent(&ctx()).await.unwrap().thinking_level, ThinkingLevel::Minimal);
    assert_eq!(retry.agent(&ctx()).await.unwrap().thinking_level, ThinkingLevel::High);
    assert_eq!(harness.conversation(retry.id, &ctx()).await.unwrap().map(|found| found.id), Some(retry.id));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ex10_registry_reload_replaces_and_wraps_tools_for_existing_conversations() {
    let registry = Arc::new(create_registry());
    let files =
        |grep: &str| Extension::new("files").tool(path_tool("read", "Read a file")).tool(path_tool("grep", grep));
    registry.install(files("Search files")).unwrap();
    let harness = open(&registry).await;
    let root = harness.root(&ctx(), CreateOptions::default()).await.unwrap();
    let tools = || async {
        let agent = root.agent(&ctx()).await.unwrap();
        agent.tools.iter().map(|tool| format!("{}: {}", tool.name(), tool.tool.description)).collect::<Vec<_>>()
    };
    registry.install(files("Search files, faster")).unwrap();
    assert_eq!(tools().await, ["read: Read a file", "grep: Search files, faster"]);
    let audit = Extension::new("audit").wrap(wrap_tool("read", |tool| {
        let mut wrapped = (*tool).clone();
        wrapped.tool.description = format!("{} (audited)", tool.tool.description);
        Ok(Arc::new(wrapped))
    }));
    registry.install(audit).unwrap();
    assert_eq!(tools().await, ["read: Read a file (audited)", "grep: Search files, faster"]);
    registry.uninstall("audit").unwrap();
    assert_eq!(tools().await, ["read: Read a file", "grep: Search files, faster"]);
    harness.close(&ctx()).await.unwrap();
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Todos {
    items: Vec<String>,
}

static TODOS: LazyLock<DocToken<Todos, RewindableConversation>> = LazyLock::new(|| {
    define_doc(DocSpec::new("example.todos", 1, RewindableConversation(DocumentFork::AsOf), Todos::default)).unwrap()
});

/// Sections and added tool names of one system message.
type SystemShape = (BTreeMap<String, String>, Option<Vec<String>>);

fn todo_extension() -> Extension {
    let parameters = json!({ "type": "object", "properties": { "item": { "type": "string" } }, "required": ["item"] });
    let todo = define_tool("todo", "Add an item to your todo list", parameters, |args, api, call| async move {
        let item = args.get("item").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (id, added) = (api.conversation_id, item.clone());
        let push = move |tx: Tx| async move { tx.doc(&TODOS, id).await?.edit(|todos| todos.items.push(added)) };
        api.commit(push, &call).await?;
        Ok(ToolExecutionResult::text(format!("added {item}")))
    });
    let todos = section(
        "todos",
        |input: PromptInput, render: Context| async move {
            let todos = input.read.snapshot(&TODOS, input.conversation_id, &render).await?;
            let items: Vec<String> =
                todos.and_then(|todos| todos.get("items")?.deserialize_into().ok()).unwrap_or_default();
            Ok((!items.is_empty()).then(|| items.join("\n")))
        },
        None,
    );
    Extension::new("todo").tool(todo).section(todos)
}

#[tokio::test]
async fn ex11_extension_state_renders_a_tool_written_document_into_the_system_prompt() {
    let setup = chat_setup(Default::default());
    setup.registry.install(todo_extension()).unwrap();
    let (harness, root) = open_chat(memory(), &setup).await;
    let call = faux_tool_call("todo", json!({ "item": "fix the build" }));
    let calling = faux_assistant_message(vec![call], FauxMessageOptions::stop_reason(pi_ai::StopReason::ToolUse));
    setup.faux.set_responses([
        calling.into(),
        support::chat::answer("Noted."),
        support::chat::answer("Working on it."),
    ]);
    let ask = |text: &str| SubmissionDraft::input(text);
    root.submit(ask("Remember to fix the build."), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let todos = harness.snapshot(&TODOS, root.id, &ctx()).await.unwrap().unwrap().to_serde();
    assert_eq!(todos, json!({ "items": ["fix the build"] }));
    root.submit(ask("What is next?"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let view = root.context(&ctx(), None).await.unwrap();
    let systems: Vec<SystemShape> = view
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::System(system) => Some((
                system
                    .sections
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(k, v)| (k, v.as_str().unwrap_or_default().to_owned()))
                    .collect(),
                system.tools_added.as_ref().map(|tools| tools.iter().map(|tool| tool.name.clone()).collect()),
            )),
            _ => None,
        })
        .collect();
    // The first request has no todos; the second shows the item the tool stored.
    let todo = BTreeMap::from([("todos".to_owned(), "<todos>\nfix the build\n</todos>".to_owned())]);
    assert_eq!(systems, [(BTreeMap::new(), Some(vec!["todo".to_owned()])), (todo, None)]);
    harness.close(&ctx()).await.unwrap();
}

fn payment_task(payments: Arc<Mutex<BTreeMap<String, u64>>>) -> Task {
    define_task(
        TaskDefinition::new("example.payment", 1, |_| Ok(Json::from(json!({ "phase": "prepare" }))))
            .phase("prepare", |task, runtime, context| async move {
                let next = json!({ "phase": "charge", "key": format!("payment-{}", task.id) });
                let next = Some(TaskState::Running { checkpoint: Json::from(next) });
                runtime.commit(move |_, _| async move { Ok(next) }, &context).await
            })
            .phase("charge", move |task, runtime, context| {
                let payments = payments.clone();
                async move {
                    let key = task.state.checkpoint().and_then(|c| c.get("key")?.as_str().map(str::to_owned)).unwrap();
                    let amount = task.input.get("amount").and_then(Json::as_u64).unwrap_or_default();
                    // An idempotent charge: a replayed phase finds the key and reuses its receipt.
                    let receipt = *payments.lock().unwrap().entry(key).or_insert(amount * 100);
                    let done = support::task_support::completed(json!({ "receipt": receipt }));
                    runtime.commit(move |_, _| async move { Ok(done) }, &context).await
                }
            })
            .abort(|_, runtime, context| async move {
                let aborted =
                    Some(TaskState::Terminal { outcome: TaskOutcome::Aborted { reason: None, result: None } });
                runtime.commit(move |_, _| async move { Ok(aborted) }, &context).await
            }),
    )
}

#[tokio::test]
async fn ex12_tasks_runs_a_two_phase_payment_to_its_receipt() {
    let payments = Arc::new(Mutex::new(BTreeMap::new()));
    let payment = payment_task(payments.clone());
    let registry = Arc::new(create_registry());
    registry.install(Extension::new("payments").task(payment.clone())).unwrap();
    let harness = open(&registry).await;
    let root = harness.root(&ctx(), CreateOptions::default()).await.unwrap();
    let id = root.id;
    let create = move |tx: Tx| async move {
        tx.create_task(payment, Json::from(json!({ "amount": 5 })), TaskOptions::conversation(id)).await
    };
    let payment_id = root.commit(create, &ctx()).await.unwrap();
    let paid = harness.wait_for_task(payment_id, &ctx()).await.unwrap();
    let outcome = serde_json::to_value(&paid.state).unwrap();
    assert_eq!(outcome["outcome"], json!({ "status": "completed", "result": { "receipt": 500 } }));
    assert_eq!(payments.lock().unwrap().len(), 1);
    harness.close(&ctx()).await.unwrap();
}
