//! Examples 29 to 31: a sandbox per conversation, a tool override, and a registry reload across a restart.

use std::sync::Mutex;

use tokio::sync::Notify;

use super::*;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Sandbox {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

static SANDBOX_DOC: LazyLock<DocToken<Sandbox, LatestConversation>> = LazyLock::new(|| {
    define_doc(DocSpec::new("app.sandbox", 1, LatestConversation(LatestFork::Initial), Sandbox::default, |_, _, _| {
        Ok(false)
    }))
    .unwrap()
});

#[tokio::test]
async fn ex29_sandbox_per_conversation_builds_each_env_from_a_document() {
    let setup = chat_setup(Default::default());
    let note = |text: &str| tool_call("write", json!({ "path": "note.txt", "content": text }));
    setup.faux.set_responses([note("from alice"), answer("Saved."), note("from bob"), answer("Saved.")]);
    setup.registry.install(coding_tools()).unwrap();
    // A conversation without a sandbox document gets no environment.
    let options = setup.options().with_env(|target, context| {
        Box::pin(async move {
            let sandbox = target.read.snapshot(&SANDBOX_DOC, target.conversation_id, &context).await?;
            let path = sandbox.and_then(|sandbox| sandbox.get("path")?.as_str().map(str::to_owned));
            Ok(path.map(|path| local_env(&path)))
        })
    });
    let harness = Harness::open(memory(), options, &ctx()).await.unwrap();
    let context = ctx();
    let conversation_for = |directory: &tempfile::TempDir| {
        let path = path_of(directory.path());
        let init = move |tx: Tx, id: ConversationId| -> futures::future::BoxFuture<'static, DurableResult<()>> {
            Box::pin(async move { tx.doc(&SANDBOX_DOC, id).await?.edit(|sandbox| sandbox.path = Some(path)) })
        };
        let options = CreateOptions::default().with_agent(AgentChange::model(faux_model())).with_init(init);
        harness.create_conversation(options, &context)
    };
    let (alice_dir, bob_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let alice = conversation_for(&alice_dir).await.unwrap();
    let bob = conversation_for(&bob_dir).await.unwrap();
    ask(&alice, "Leave a note.").await;
    ask(&bob, "Leave a note.").await;
    assert_eq!(std::fs::read_to_string(alice_dir.path().join("note.txt")).unwrap(), "from alice");
    assert_eq!(std::fs::read_to_string(bob_dir.path().join("note.txt")).unwrap(), "from bob");
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ex30_tool_override_replaces_bash_for_one_conversation_and_the_wrap_times_both() {
    let project = tempfile::tempdir().unwrap();
    let project_path = path_of(project.path());
    std::fs::create_dir_all(project.path().join(".venv/bin")).unwrap();
    std::fs::write(project.path().join(".venv/bin/activate"), format!("export VIRTUAL_ENV=\"{project_path}/.venv\"\n"))
        .unwrap();
    let setup = chat_setup(Default::default());
    let probe = || tool_call("bash", json!({ "command": "echo venv: $VIRTUAL_ENV" }));
    setup.faux.set_responses([probe(), answer("Done."), probe(), answer("Done.")]);
    let timings = Arc::new(Mutex::new(Vec::new()));
    let sink = timings.clone();
    let timing = Extension::new("timing").wrap(wrap_tool("bash", move |bash| {
        let (mut timed, inner, sink) = ((*bash).clone(), bash.execute.clone(), sink.clone());
        timed.execute = Arc::new(move |args, api, context| {
            let (inner, sink) = (inner.clone(), sink.clone());
            Box::pin(async move {
                let start = std::time::Instant::now();
                let result = inner(args, api, context).await;
                sink.lock().unwrap().push(start.elapsed());
                result
            })
        });
        Ok(Arc::new(timed))
    }));
    let coding = Arc::new(coding_tools());
    let timing = Arc::new(timing);
    setup.registry.install(coding.clone()).unwrap();
    setup.registry.install(timing.clone()).unwrap();
    let venv =
        create_bash_tool(BashToolOptions { command_prefix: Some("source .venv/bin/activate".into()), prepare: None });
    setup.registry.install(Extension::new("venv").tool(venv)).unwrap();
    setup.settings.lock().unwrap().extensions = Some(vec![coding, timing]);
    let options = setup.options().with_shared_env(local_env(&project_path));
    let (harness, plain) = open_chat_with(memory(), &setup, options).await;
    let add = AgentChange {
        extensions: Change::Set(ExtensionSelection::Edit { add: Some(vec!["venv".into()]), remove: None }),
        ..AgentChange::model(faux_model())
    };
    let python = harness.create_conversation(CreateOptions::default().with_agent(add), &ctx()).await.unwrap();
    ask(&plain, "Which venv?").await;
    let plain_output = last_result(&plain).await;
    assert!(plain_output.starts_with("venv:") && !plain_output.contains(&project_path), "{plain_output}");
    ask(&python, "Which venv?").await;
    assert_eq!(last_result(&python).await.replace(&project_path, "<project>"), "venv: <project>/.venv");
    assert_eq!(timings.lock().unwrap().len(), 2);
    harness.close(&ctx()).await.unwrap();
}

/// An extension whose `version` tool reports `version`; it signals `started` and waits for `gate` first.
fn versioned(version: &str, started: Arc<Notify>, gate: Arc<Notify>, gated: bool) -> Extension {
    let version = version.to_owned();
    let tool =
        define_tool("version", "Report the tool's code version", support::chat::no_parameters(), move |_, _, _| {
            let (version, started, gate) = (version.clone(), started.clone(), gate.clone());
            async move {
                started.notify_one();
                if gated {
                    gate.notified().await;
                }
                Ok(ToolExecutionResult::text(version))
            }
        });
    Extension::new("versioned").tool(tool)
}

#[tokio::test]
async fn ex31_reload_and_restart_keeps_running_calls_and_reselects_installed_code() {
    let setup = chat_setup(Default::default());
    let call = || tool_call("version", json!({}));
    setup.faux.set_responses([call(), answer("Done."), call(), answer("Done.")]);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    let (started, gate) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let open = |registry: Arc<Registry>| {
        let path = path.clone();
        let mut options = setup.options();
        options.registry = registry as Arc<dyn RegistryReader>;
        async move { Harness::open(Arc::new(open_sqlite_storage(&path).await.unwrap()), options, &ctx()).await.unwrap() }
    };
    let registry = Arc::new(create_registry());
    registry.install(versioned("v1", started.clone(), gate.clone(), true)).unwrap();
    let harness = open(registry.clone()).await;
    let agent = AgentChange {
        extensions: Change::Set(ExtensionSelection::List(vec!["versioned".into()])),
        ..AgentChange::model(faux_model())
    };
    let root = harness.root(&ctx(), CreateOptions::default().with_agent(agent)).await.unwrap();
    let submission = root.submit(SubmissionDraft::input("Which version?"), &ctx()).await.unwrap();
    started.notified().await;
    // A reload during the call: the running call keeps the code it started with.
    registry.install(versioned("v2", started.clone(), gate.clone(), false)).unwrap();
    gate.notify_one();
    submission.wait(&ctx()).await.unwrap();
    assert_eq!(last_result(&root).await, "v1");
    ask(&root, "And now?").await;
    assert_eq!(last_result(&root).await, "v2");
    harness.close(&ctx()).await.unwrap();

    // After a restart the stored selection names `versioned`, which resolves once something installs it.
    let registry = Arc::new(create_registry());
    let harness = open(registry.clone()).await;
    let root = harness.root(&ctx(), CreateOptions::default()).await.unwrap();
    let tools = || async { tool_names(&root.agent(&ctx()).await.unwrap()) };
    assert!(tools().await.is_empty());
    registry.install(versioned("v3", started, gate, false)).unwrap();
    assert_eq!(tools().await, ["version"]);
    harness.close(&ctx()).await.unwrap();
}
