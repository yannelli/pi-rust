//! Port of upstream `test/harness-reports.test.ts`: what each report to `HarnessOptions::on_report` says it is
//! about.

mod support;

use std::sync::{Arc, Mutex};

use pi_chord::Json;
use pi_durable::harness::generation::{GenerationHooks, generation_task};
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::tasks::TaskOptions;
use pi_durable::testing::faux::{FauxMessageOptions, faux_assistant_message, faux_text};
use pi_durable::*;
use serde_json::{Value, json};
use support::harness_support::{chat_setup, faux_model};
use support::task_support::{completed, eventually};
use support::{ControlledStorage, Deferred, commit_observer, context};

type Reports = Arc<Mutex<Vec<(String, ReportInfo)>>>;

fn collect(options: HarnessOptions) -> (HarnessOptions, Reports) {
    let reports: Reports = Arc::default();
    let sink = reports.clone();
    let options =
        options.with_on_report(move |report| sink.lock().unwrap().push((message(&report.error), report.info.clone())));
    (options, reports)
}

/// The message of an error without the kind prefix `Display` adds.
fn message(error: &DurableError) -> String {
    error.to_string().trim_start_matches("Error: ").to_owned()
}

fn info_of(reports: &Reports, text: &str) -> Option<ReportInfo> {
    let reports = reports.lock().unwrap();
    reports.iter().find(|(message, _)| message == text).map(|(_, info)| info.clone())
}

/// Open a Harness over `storage` whose registry installs `tasks` as extension `test`.
async fn open(
    storage: Arc<dyn Storage>,
    tasks: Vec<Task>,
    configure: impl FnOnce(HarnessOptions) -> HarnessOptions,
) -> (Harness, Arc<Registry>, Reports) {
    let registry = Arc::new(create_registry());
    if !tasks.is_empty() {
        registry.install(tasks.into_iter().fold(Extension::new("test"), Extension::task)).unwrap();
    }
    let options = HarnessOptions::new(chat_setup(Default::default()).models, registry.clone());
    let (options, reports) = collect(configure(options));
    let harness = Harness::open(storage, options, &context()).await.unwrap();
    (harness, registry, reports)
}

async fn create(harness: &Harness, task: &Task) -> TaskId {
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let (task, id) = (task.clone(), root.id);
    let create = move |tx: Tx| async move { tx.create_task(task, Json::Null, TaskOptions::conversation(id)).await };
    root.commit(create, &context()).await.unwrap()
}

fn run_phase(name: &str) -> TaskDefinition {
    TaskDefinition::new(name, 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
}

#[tokio::test]
async fn say_which_task_reported_a_failure_or_faulted() {
    let faulty = define_task(
        run_phase("test.faulty")
            .phase("run", |_, runtime, _| async move {
                runtime.report(&DurableError::error("noted"));
                Err(DurableError::error("broken"))
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let (harness, _, reports) = open(Arc::new(MemoryStorage::new()), vec![faulty.clone()], |options| options).await;
    let id = create(&harness, &faulty).await;
    harness.resume().unwrap();
    let settled = harness.wait_for_task(id, &context()).await.unwrap();
    assert!(matches!(settled.state.outcome(), Some(TaskOutcome::Faulted { .. })));
    eventually(|| async { reports.lock().unwrap().len() == 2 }).await;
    // The task's own failures name the extension that installed it.
    let task = ReportTask { id, kind: "test.faulty".into(), conversation_id: ROOT_CONVERSATION_ID };
    let extension = Some("test".to_owned());
    assert_eq!(
        *reports.lock().unwrap(),
        [
            ("noted".to_owned(), ReportInfo::TaskError { task: task.clone(), extension: extension.clone() }),
            ("broken".to_owned(), ReportInfo::TaskFaulted { task, extension, reason: FaultReason::Threw }),
        ]
    );
    // Hosts see upstream's JSON shape.
    let faulted = serde_json::to_value(&reports.lock().unwrap()[1].1).unwrap();
    let task = json!({ "id": id.0, "kind": "test.faulty", "conversationId": ROOT_CONVERSATION_ID.0 });
    assert_eq!(faulted, json!({ "kind": "task_faulted", "task": task, "extension": "test", "reason": "threw" }));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn name_the_extension_whose_definition_ran_also_after_another_one_replaced_it() {
    let (gate, started) = (Deferred::default(), Arc::new(Deferred::default()));
    let definition = |name: &'static str| {
        let (gate, started) = (gate.clone(), started.clone());
        define_task(
            run_phase("test.replaced")
                .phase("run", move |_, _, _| {
                    let (gate, started) = (gate.clone(), started.clone());
                    async move {
                        started.resolve();
                        gate.wait().await;
                        Err(DurableError::error(format!("broken in {name}")))
                    }
                })
                .abort(|_, _, _| async { Ok(()) }),
        )
    };
    let old = definition("old");
    let (harness, registry, reports) = open(Arc::new(MemoryStorage::new()), Vec::new(), |options| options).await;
    registry.install(Extension::new("old-extension").task(old.clone())).unwrap();
    let id = create(&harness, &old).await;
    harness.resume().unwrap();
    started.wait().await;
    registry.uninstall("old-extension").unwrap();
    registry.install(Extension::new("new-extension").task(definition("new"))).unwrap();
    gate.resolve();
    harness.wait_for_task(id, &context()).await.unwrap();
    let faulted = |reports: &Reports| {
        let reports = reports.lock().unwrap();
        reports.iter().find(|(_, info)| matches!(info, ReportInfo::TaskFaulted { .. })).cloned()
    };
    eventually(|| async { faulted(&reports).is_some() }).await;
    let (message, info) = faulted(&reports).unwrap();
    assert_eq!(message, "broken in old");
    let ReportInfo::TaskFaulted { extension, .. } = info else { unreachable!() };
    assert_eq!(extension.as_deref(), Some("old-extension"));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn name_the_extension_of_a_failing_section_or_hook_and_tell_them_from_a_failing_environment_builder() {
    let setup = chat_setup(Default::default());
    setup.faux.set_responses([faux_assistant_message(vec![faux_text("answer")], FauxMessageOptions::default())]);
    let broken = section("broken", |_, _| async { Err(DurableError::error("section")) }, None);
    setup.registry.install(Extension::new("sections").section(broken)).unwrap();
    let hooks = GenerationHooks {
        after_response: Some(Arc::new(|_, _, _| Box::pin(async { Err(DurableError::error("hook")) }))),
        ..GenerationHooks::default()
    };
    setup.registry.install(Extension::new("hooks").hook(hook(&generation_task(), hooks))).unwrap();
    let options = setup.options().with_env(|_, _| Box::pin(async { Err(DurableError::error("env")) }));
    let (options, reports) = collect(options);
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context()).await.unwrap();
    let agent = CreateOptions::default().with_agent(AgentChange::model(faux_model()));
    let root = harness.root(&context(), agent).await.unwrap();
    let submission = root.submit(SubmissionDraft::input("hi"), &context()).await.unwrap();
    submission.wait(&context()).await.unwrap();
    let generation = |task: &Option<ReportTask>| {
        task.as_ref().is_some_and(|task| task.kind == "pi.generation" && task.conversation_id == root.id)
    };
    match info_of(&reports, "env") {
        Some(ReportInfo::HostError { callback: HostCallback::Env, task }) => assert!(generation(&task)),
        other => panic!("{other:?}"),
    }
    for (text, name) in [("section", "sections"), ("hook", "hooks")] {
        match info_of(&reports, text) {
            Some(ReportInfo::ExtensionError { extension, task, conversation_id: None }) => {
                assert_eq!(extension.as_deref(), Some(name));
                assert!(generation(&task));
            }
            other => panic!("{other:?}"),
        }
    }
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn tell_extension_listener_wake_and_timer_failures_from_a_failed_session() {
    let sleeper = define_task(
        run_phase("test.sleeper")
            .phase("run", |_, runtime, context| async move {
                // The host's set_timer declines a timer that paces work: reported, and armed with Tokio instead.
                drop(runtime.set_timer(|| {}, 10.0));
                let until = runtime.now() + 60_000.0;
                runtime.sleep_until(until, Json::from(json!({ "phase": "done" })), &context).await
            })
            .phase("done", |_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let storage = Arc::new(ControlledStorage::default());
    let (harness, registry, reports) = open(storage.clone(), vec![sleeper.clone()], |options| {
        options.with_on_wake(|_| Err(DurableError::error("alarm"))).with_set_timer(|_, _, _| None)
    })
    .await;
    let echo = define_tool("echo", "echo", json!({ "type": "object" }), |_, _, _| async {
        Ok(ToolExecutionResult::default())
    });
    registry.install(Extension::new("tools").tool(echo)).unwrap();
    let rename = wrap_tool("echo", |tool| {
        let mut renamed = (*tool).clone();
        renamed.tool.name = "renamed".into();
        Ok(Arc::new(renamed))
    });
    registry.install(Extension::new("wraps").wrap(rename)).unwrap();
    let _listener = harness.observe_commits(commit_observer(|_, _| Err(DurableError::error("listener")))).unwrap();
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    root.agent(&context()).await.unwrap();
    create(&harness, &sleeper).await;
    harness.resume().unwrap();
    let declined = "HarnessOptions::set_timer declined a timer that paces work; using a Tokio timer";
    eventually(|| async { info_of(&reports, "alarm").is_some() && info_of(&reports, declined).is_some() }).await;
    assert_eq!(
        info_of(&reports, "Wrapper renamed echo to renamed"),
        Some(ReportInfo::ExtensionError {
            extension: Some("wraps".into()),
            task: None,
            conversation_id: Some(root.id)
        })
    );
    assert_eq!(info_of(&reports, "listener"), Some(ReportInfo::ListenerError));
    assert_eq!(info_of(&reports, "alarm"), Some(ReportInfo::HostError { callback: HostCallback::OnWake, task: None }));
    assert_eq!(
        info_of(&reports, declined),
        Some(ReportInfo::HostError { callback: HostCallback::SetTimer, task: None })
    );

    storage.fail_next_commit(DurableError::error("disk gone"));
    let create_conversation = |tx: Tx| async move { tx.create_conversation(ConversationOwnership::Ownerless).await };
    let _ = root.commit(create_conversation, &context()).await;
    harness.closed().await;
    assert_eq!(info_of(&reports, "disk gone"), Some(ReportInfo::StorageFailed));
}

#[test]
fn names_every_report_by_the_kind_it_serializes() {
    let task = ReportTask { id: TaskId(1), kind: "test.task".into(), conversation_id: ROOT_CONVERSATION_ID };
    let infos = [
        ReportInfo::StorageFailed,
        ReportInfo::TaskFaulted { task: task.clone(), extension: None, reason: FaultReason::Threw },
        ReportInfo::TaskInterrupted {
            task: task.clone(),
            extension: None,
            reason: InterruptionReason::Interrupted,
            restarts: 1,
        },
        ReportInfo::MigrationFailed { task: task.clone(), extension: None },
        ReportInfo::TaskOutdated { task: task.clone(), extension: None, reason: OutdatedReason::TaskTooOld },
        ReportInfo::TaskError { task, extension: None },
        ReportInfo::ExtensionError { extension: None, task: None, conversation_id: None },
        ReportInfo::HostError { callback: HostCallback::Now, task: None },
        ReportInfo::ListenerError,
    ];
    let kinds: Vec<&str> = infos.iter().map(ReportInfo::kind).collect();
    let tags: Vec<Value> = infos.iter().map(|info| serde_json::to_value(info).unwrap()["kind"].clone()).collect();
    assert_eq!(tags, kinds.iter().map(|kind| json!(kind)).collect::<Vec<_>>());
    let expected = [
        "storage_failed",
        "task_faulted",
        "task_interrupted",
        "migration_failed",
        "task_outdated",
        "task_error",
        "extension_error",
        "host_error",
        "listener_error",
    ];
    assert_eq!(kinds, expected);
}
