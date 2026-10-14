//! Examples 26 to 31: a coding agent with live settings, plan mode, a reviewer loop, a sandbox per conversation,
//! a tool override, and a registry reload across a restart.

use std::path::Path;
use std::sync::Mutex;

use pi_ai::{Message, StopReason, UserContent};
use pi_durable::entries::{ASSISTANT_ENTRY, TOOL_RESULT_ENTRY};
use pi_durable::env::{LocalEnvOptions, LocalExecutionEnv, SharedEnv};
use pi_durable::harness::generation::{GenerationHooks, generation_task};
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxModelDefinition, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message,
    faux_tool_call,
};
use pi_durable::tools::{BashToolOptions, ReadToolOptions, coding_tools, create_bash_tool, create_read_tool};
use support::chat::answer;

use super::*;

fn tool_call(name: &str, args: Value) -> FauxResponseStep {
    faux_assistant_message(vec![faux_tool_call(name, args)], FauxMessageOptions::stop_reason(StopReason::ToolUse))
        .into()
}

fn local_env(cwd: &str) -> SharedEnv {
    Arc::new(LocalExecutionEnv::new(LocalEnvOptions::new(cwd)))
}

/// The canonical path, without the `\\?\` prefix Windows adds.
fn path_of(path: &Path) -> String {
    let path = path.canonicalize().unwrap().to_string_lossy().into_owned();
    path.strip_prefix(r"\\?\").map_or(path.clone(), str::to_owned)
}

/// How `pwd` prints a path: Git Bash on Windows shows `C:\a\b` as `/c/a/b`.
fn shell_path(path: &str) -> String {
    match path.split_once(":\\") {
        Some((drive, rest)) if cfg!(windows) => format!("/{}/{}", drive.to_lowercase(), rest.replace('\\', "/")),
        _ => path.to_owned(),
    }
}

async fn ask(conversation: &Conversation, text: &str) {
    conversation.submit(SubmissionDraft::input(text), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
}

/// The text of the newest tool result, each part trimmed.
async fn last_result(conversation: &Conversation) -> String {
    let page = conversation.entries(EntryRange::default(), 10, None, &ctx()).await.unwrap();
    let entry = page.items.into_iter().find(|entry| TOOL_RESULT_ENTRY.is(Some(entry))).unwrap();
    match entry.model.unwrap_or_default().first() {
        Some(Message::ToolResult(result)) => {
            result.content.iter().filter_map(pi_ai::ContentBlock::as_text).map(str::trim).collect()
        }
        _ => panic!("tool result entries hold one tool result"),
    }
}

fn coding_name() -> String {
    coding_tools().name
}

#[tokio::test]
async fn ex26_coding_agent_follows_cwd_changes_and_live_settings() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join("app")).unwrap();
    let root_path = path_of(workspace.path());
    let setup = chat_setup(Default::default());
    setup.registry.install(coding_tools()).unwrap();
    let preamble = "You are a coding agent. Use the tools to inspect the project.";
    let cwd =
        section("cwd", |input: PromptInput, _| std::future::ready(Ok(input.env.map(|env| env.cwd().to_owned()))), None);
    let text = section("preamble", move |_, _| std::future::ready(Ok(Some(preamble.to_owned()))), Some(false));
    setup.registry.install(Extension::new("coding").section(text).section(cwd)).unwrap();
    // The application's own settings, read at each use through the settings source.
    let parallel_tools = Arc::new(Mutex::new(true));
    let source = parallel_tools.clone();
    let mut options = setup.options();
    options.settings = Some(Arc::new(move || {
        let mode = if *source.lock().unwrap() { ToolExecutionMode::Parallel } else { ToolExecutionMode::Sequential };
        let retry = RetryOverrides { max_retries: Some(3), ..Default::default() };
        Ok(HarnessSettings { tool_execution: Some(mode), retry: Some(retry), ..Default::default() })
    }));
    let fallback = root_path.clone();
    let options = options.with_env(move |target, _| {
        let env = local_env(&target.cwd.unwrap_or_else(|| fallback.clone()));
        Box::pin(async move { Ok(Some(env)) })
    });
    let pwd = || tool_call("bash", json!({ "command": "pwd" }));
    setup.faux.set_responses([pwd(), answer("Done."), pwd(), answer("Done.")]);
    let (harness, root) = open_chat_with(memory(), &setup, options).await;
    root.configure(AgentChange { cwd: Change::Set(root_path.clone()), ..Default::default() }, &ctx()).await.unwrap();
    ask(&root, "Where are we?").await;
    assert_eq!(last_result(&root).await, shell_path(&root_path));
    let app = format!("{root_path}/app");
    root.configure(AgentChange { cwd: Change::Set(app.clone()), ..Default::default() }, &ctx()).await.unwrap();
    *parallel_tools.lock().unwrap() = false;
    ask(&root, "And now?").await;
    assert_eq!(last_result(&root).await, shell_path(&app));
    harness.close(&ctx()).await.unwrap();
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Plan {
    steps: Vec<String>,
}

static PLAN_DOC: LazyLock<DocToken<Plan, RewindableConversation>> = LazyLock::new(|| {
    define_doc(DocSpec::new("app.plan", 1, RewindableConversation(DocumentFork::AsOf), Plan::default)).unwrap()
});

fn plan_extension() -> Extension {
    let parameters =
        json!({ "type": "object", "properties": { "steps": { "type": "array", "items": { "type": "string" } } } });
    let submit =
        define_tool("submit_plan", "Submit the plan as a list of steps.", parameters, |args, api, call| async move {
            let steps: Vec<String> =
                serde_json::from_value(args.get("steps").cloned().unwrap_or_default()).unwrap_or_default();
            let id = api.conversation_id;
            api.commit(
                move |tx: Tx| async move { tx.doc(&PLAN_DOC, id).await?.edit(|plan| plan.steps = steps) },
                &call,
            )
            .await?;
            let control = ToolControl { terminate: Some(true), ..Default::default() };
            Ok(ToolExecutionResult { control: Some(control), ..ToolExecutionResult::text("Plan submitted.") })
        });
    let text = "You are in plan mode. Read the code, then call submit_plan. Change nothing.";
    let mode = section("plan_mode", move |_, _| std::future::ready(Ok(Some(text.to_owned()))), None);
    Extension::new("plan").tool(submit).section(mode)
}

/// Sections, added tool names, and removed tool names of each system message in the context.
async fn system_messages(conversation: &Conversation) -> Vec<Value> {
    let view = conversation.context(&ctx(), None).await.unwrap();
    let systems = view.messages.iter().filter_map(|message| match message {
        Message::System(system) => Some(json!({
            "sections": system.sections,
            "added": system.tools_added.as_ref().map(|tools| tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>()),
            "removed": system.tools_removed.as_ref().map(|tools| tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>()),
        })),
        _ => None,
    });
    systems.collect()
}

#[tokio::test]
async fn ex27_plan_mode_narrows_tools_and_terminates_on_the_submitted_plan() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("server.ts"), "app.listen(3000);\n").unwrap();
    let setup = chat_setup(Default::default());
    let steps = json!({ "steps": ["Read PORT from the environment", "Default to 3000"] });
    setup.faux.set_responses([
        tool_call("read", json!({ "path": "server.ts" })),
        tool_call("submit_plan", steps),
        answer("Implementing step 1."),
    ]);
    let coding = Arc::new(coding_tools());
    setup.registry.install(coding.clone()).unwrap();
    setup.registry.install(plan_extension()).unwrap();
    setup.settings.lock().unwrap().extensions = Some(vec![coding]);
    let options = setup.options().with_shared_env(local_env(&path_of(directory.path())));
    let (harness, root) = open_chat_with(memory(), &setup, options).await;
    let tools = || async { tool_names(&root.agent(&ctx()).await.unwrap()) };
    assert_eq!(tools().await, ["read", "write", "edit", "bash"]);
    let enter = AgentChange {
        extensions: Change::Set(ExtensionSelection::Edit { add: Some(vec!["plan".into()]), remove: None }),
        tools: Change::Set(ToolFilter::List(vec![
            create_read_tool(ReadToolOptions::default()).name().to_owned(),
            "submit_plan".into(),
        ])),
        ..Default::default()
    };
    root.configure(enter, &ctx()).await.unwrap();
    assert_eq!(tools().await, ["read", "submit_plan"]);
    ask(&root, "Make the port configurable.").await;
    let plan = harness.snapshot(&PLAN_DOC, root.id, &ctx()).await.unwrap().unwrap().to_serde();
    assert_eq!(plan["steps"], json!(["Read PORT from the environment", "Default to 3000"]));
    root.configure(AgentChange { extensions: Change::Clear, tools: Change::Clear, ..Default::default() }, &ctx())
        .await
        .unwrap();
    assert_eq!(tools().await, ["read", "write", "edit", "bash"]);
    ask(&root, "Go ahead.").await;
    // The first request already runs in plan mode; leaving it patches the section away and restores the tools.
    let plan_mode =
        "<plan_mode>\nYou are in plan mode. Read the code, then call submit_plan. Change nothing.\n</plan_mode>";
    let expected = [
        json!({ "sections": { "plan_mode": plan_mode }, "added": ["read", "submit_plan"], "removed": null }),
        json!({ "sections": { "plan_mode": null }, "added": ["write", "edit", "bash"], "removed": ["submit_plan"] }),
    ];
    assert_eq!(system_messages(&root).await, expected);
    harness.close(&ctx()).await.unwrap();
}

const DONE: &str = "No further findings.";

fn reviewer_extension() -> Extension {
    let role = "You review diffs. Report problems as a list. Never edit files.";
    let role = section("role", move |_, _| std::future::ready(Ok(Some(role.to_owned()))), None);
    let hooks = GenerationHooks {
        on_yield: Some(Arc::new(|answer, _, _| {
            let text: String = answer.content.iter().filter_map(pi_ai::ContentBlock::as_text).collect();
            let more = format!("Look again for anything you missed. Say \"{DONE}\" when there is nothing left.");
            let next = (!text.contains(DONE)).then(|| UserContent::Text(more));
            Box::pin(async move { Ok(next) })
        })),
        ..GenerationHooks::default()
    };
    Extension::new("reviewer").section(role).hook(hook(&generation_task(), hooks))
}

#[tokio::test]
async fn ex28_reviewer_continues_until_its_answer_says_it_is_done() {
    let worktree = tempfile::tempdir().unwrap();
    std::fs::write(worktree.path().join("user.ts"), "export const name = (user) => user.name;\n").unwrap();
    let worktree_path = path_of(worktree.path());
    let models = vec![FauxModelDefinition::new("big"), FauxModelDefinition::new("small")];
    let setup = chat_setup(RegisterFauxProviderOptions { models, ..Default::default() });
    setup.faux.set_responses([
        tool_call("read", json!({ "path": "user.ts" })),
        answer("1. `name` does not handle a missing user."),
        answer(&format!("2. `user` has no type. {DONE}")),
    ]);
    let coding = Arc::new(coding_tools());
    setup.registry.install(coding.clone()).unwrap();
    setup.registry.install(reviewer_extension()).unwrap();
    setup.settings.lock().unwrap().extensions = Some(vec![coding]);
    let options = setup.options().with_env(|target, _| {
        let env = local_env(&target.cwd.unwrap_or_else(|| ".".into()));
        Box::pin(async move { Ok(Some(env)) })
    });
    let harness = Harness::open(memory(), options, &ctx()).await.unwrap();
    let big = ModelRef { provider: "faux".into(), model_id: "big".into() };
    harness.root(&ctx(), CreateOptions::default().with_agent(AgentChange::model(big))).await.unwrap();
    let small = ModelRef { provider: "faux".into(), model_id: "small".into() };
    let agent = AgentChange {
        extensions: Change::Set(ExtensionSelection::List(vec![coding_name(), "reviewer".into()])),
        tools: Change::Set(ToolFilter::List(vec!["read".into()])),
        cwd: Change::Set(worktree_path.clone()),
        ..AgentChange::model(small)
    };
    let reviewer = harness.create_conversation(CreateOptions::default().with_agent(agent), &ctx()).await.unwrap();
    let agent = reviewer.agent(&ctx()).await.unwrap();
    let extensions: Vec<String> = agent.extensions.iter().map(|extension| extension.name.clone()).collect();
    assert_eq!(agent.model.as_ref().map(|model| model.model_id.as_str()), Some("small"));
    assert_eq!(
        (extensions, tool_names(&agent), agent.cwd),
        (vec![coding_name(), "reviewer".into()], vec!["read".into()], Some(worktree_path))
    );
    ask(&reviewer, "Review user.ts.").await;
    let lines: Vec<String> = all_entries(&reviewer)
        .await
        .into_iter()
        .filter_map(|entry| match entry.model.as_ref()?.first()? {
            Message::User(_) => Some(format!("> {}", text_of(entry.model.as_ref()?.first())?)),
            message if ASSISTANT_ENTRY.is(Some(&entry)) => {
                Some(format!("reviewer: {}", text_of(Some(message)).filter(|text| !text.is_empty())?))
            }
            _ => None,
        })
        .collect();
    let expected = [
        "> Review user.ts.".to_owned(),
        "reviewer: 1. `name` does not handle a missing user.".to_owned(),
        format!("> Look again for anything you missed. Say \"{DONE}\" when there is nothing left."),
        format!("reviewer: 2. `user` has no type. {DONE}"),
    ];
    assert_eq!(lines, expected);
    harness.close(&ctx()).await.unwrap();
}

#[path = "coding_more.rs"]
mod more;
