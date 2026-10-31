//! Port of upstream `test/spec-usage.test.ts`: the usage examples of the spec, compile-checked. The names the spec
//! leaves to the application are parameters or stubs; nothing here runs. Upstream's live settings getters become a
//! settings source closure, and declared tasks such as `Anchor` are small definitions.
#![allow(dead_code, clippy::too_many_arguments)]

mod support;

use std::sync::{Arc, LazyLock, Mutex};

use crate::support::{ValueCall, on_value};
use pi_ai::{AssistantMessage, ToolCall, UserContent};
use pi_chord::{Context, Json};
use pi_durable::documents::*;
use pi_durable::env::{LocalEnvOptions, LocalExecutionEnv, SharedEnv};
use pi_durable::harness::generation::{GenerationHooks, generation_task};
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::tool::{BeforeToolDecision, ToolHooks, tool_task};
use pi_durable::harness::*;
use pi_durable::models::Models;
use pi_durable::session::{Session, Tx};
use pi_durable::tasks::TaskOptions;
use pi_durable::tools::{
    BashToolOptions, ReadToolOptions, coding_tools, create_bash_tool, create_edit_tool, create_read_tool,
};
use pi_durable::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

// ─── Names the spec leaves to the application ────────────────────────────────

fn render_agents_md() -> Option<String> {
    None
}
fn render_skills() -> Option<String> {
    None
}
fn is_dangerous(_: &ToolCall) -> bool {
    false
}
fn writes(_: &ToolCall) -> bool {
    false
}
async fn request_second_pass(_: AssistantMessage, _: HookApi, _: Context) -> DurableResult<Option<UserContent>> {
    Ok(None)
}
async fn answer_text(_: &ToolExecutionApi, _: EntryId, _: &Context) -> DurableResult<String> {
    Ok(String::new())
}
fn new_key() -> String {
    String::new()
}
fn local_env(cwd: &str) -> SharedEnv {
    Arc::new(LocalExecutionEnv::new(LocalEnvOptions::new(cwd)))
}
async fn container_env(_image: &str, cwd: &str, _: &Context) -> DurableResult<Option<SharedEnv>> {
    Ok(Some(local_env(cwd)))
}
fn record_metric(_: &str, _: u128) {}

fn idle_task(name: &str) -> Task {
    define_task(
        TaskDefinition::new(name, 1, |_| Ok(Json::from(json!({ "phase": "hold" }))))
            .phase("hold", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) }),
    )
}

// ─── Section 7.1: extensions and host setup ──────────────────────────────────

fn text_section(key: &str, render: fn() -> Option<String>, tag: Option<bool>) -> Arc<PromptSection> {
    section(key, move |_, _| std::future::ready(Ok(render())), tag)
}

fn extensions() -> Vec<Extension> {
    let context_files = Extension::new("context-files").section(text_section("agents-md", render_agents_md, None));
    let skills = Extension::new("skills").section(text_section("skills", render_skills, None));
    let preamble =
        |_: PromptInput, _: Context| std::future::ready(Ok(Some("You are an expert coding assistant.".into())));
    let coding = Extension::new("coding").section(section("preamble", preamble, Some(false))).section(section(
        "cwd",
        // The environment the host built for this conversation, in the conversation's directory.
        |input: PromptInput, _| {
            std::future::ready(Ok(input.env.map(|env| format!("Working directory: {}", env.cwd()))))
        },
        None,
    ));
    let permissions = Extension::new("permissions").hook(hook(
        &tool_task(),
        ToolHooks {
            before_tool: Some(Arc::new(|call, _, _| {
                let block = is_dangerous(&call.call).then(|| BeforeToolDecision {
                    block: Some("Needs approval".into()),
                    ..BeforeToolDecision::default()
                });
                Box::pin(async move { Ok(block) })
            })),
            after_tool: None,
        },
    ));
    // A role and a review loop, for conversations that select it.
    let role = |_: PromptInput, _: Context| {
        std::future::ready(Ok(Some("You review diffs. Report problems as a list. Never edit files.".into())))
    };
    let reviewer = Extension::new("reviewer").section(section("role", role, None)).hook(hook(
        &generation_task(),
        GenerationHooks {
            on_yield: Some(Arc::new(|answer, api, context| Box::pin(request_second_pass(answer, api, context)))),
            ..GenerationHooks::default()
        },
    ));
    let timing = Extension::new("timing").wrap(wrap_tool("bash", |tool| {
        let mut timed = (*tool).clone();
        let inner = tool.execute.clone();
        timed.execute = Arc::new(move |args, api, context| {
            let inner = inner.clone();
            Box::pin(async move {
                let start = std::time::Instant::now();
                let result = inner(args, api, context).await;
                record_metric("bash", start.elapsed().as_millis());
                result
            })
        });
        Ok(Arc::new(timed))
    }));
    // A bash inside a Python virtualenv for one conversation: it replaces the coding tools' bash in place, and
    // Timing, if selected, wraps it.
    let options = BashToolOptions { command_prefix: Some("source .venv/bin/activate".into()), prepare: None };
    let venv = Extension::new("venv").tool(create_bash_tool(options));
    vec![context_files, skills, coding, permissions, reviewer, timing, venv]
}

// ─── Section 7.2: hooks reading extension state ──────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct PlanMode {
    enabled: bool,
}

static PLAN_MODE_DOC: LazyLock<DocToken<PlanMode, LatestConversation>> = LazyLock::new(|| {
    define_doc(DocSpec::new(
        "app.plan-mode",
        1,
        LatestConversation(LatestFork::Current),
        PlanMode::default,
        |_, _, _| Ok(false),
    ))
    .unwrap()
});

fn plan_mode() -> Extension {
    let before_tool = Arc::new(|call: tool::ToolHookCall, api: HookApi, context: Context| {
        Box::pin(async move {
            // An absent document means plan mode is off.
            let state = api.snapshot(&PLAN_MODE_DOC, api.conversation_id(), &context).await?;
            let enabled = state.and_then(|state| state.get("enabled").and_then(Json::as_bool)).unwrap_or(false);
            let block = BeforeToolDecision { block: Some("Plan mode: read-only".into()), ..Default::default() };
            Ok((enabled && writes(&call.call)).then_some(block))
        }) as pi_chord::BoxFuture<_>
    });
    Extension::new("plan-mode").hook(hook(&tool_task(), ToolHooks { before_tool: Some(before_tool), after_tool: None }))
}

// ─── Section 7.3: subagents ──────────────────────────────────────────────────

fn subagent() -> Extension {
    let parameters = json!({ "type": "object", "properties": { "task": { "type": "string" } }, "required": ["task"] });
    let tool = define_tool(
        "subagent",
        "Delegate a self-contained task to a subagent and get its answer back.",
        parameters,
        |args, api: ToolExecutionApi, context| async move {
            let task = args.get("task").and_then(|task| task.as_str()).unwrap_or_default().to_owned();
            let owner = api.task_id;
            let child = api
                .commit(
                    move |tx: Tx| async move {
                        let query = ConversationQuery { owner_task_id: Some(owner), ..ConversationQuery::default() };
                        if let Some(existing) = tx.scan_conversations(query, 1, None).await?.items.first() {
                            return Ok(existing.id);
                        }
                        // Starts as a copy of this conversation's agent: model, thinking level, cwd, extensions, tools.
                        let created = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
                        // Without this extension, the child is not offered this tool.
                        let remove = ExtensionSelection::Edit { add: None, remove: Some(vec!["subagent".into()]) };
                        let change = AgentChange { extensions: Change::Set(remove), ..AgentChange::default() };
                        configure(&tx, created.id, &change).await?;
                        Ok(created.id)
                    },
                    &context,
                )
                .await?;
            api.details(Json::from(json!({ "conversationId": child })), &context).await?;
            let handle = api.conversation(child, &context).await?.expect("the child conversation");
            let request = SubmissionDraft::input(task).with_request_id(format!("subagent:{owner}"));
            let settled = handle.submit(request, &context).await?.wait(&context).await?;
            let (SubmissionStatus::Done, Some(answer)) = (settled.status, settled.answer) else {
                return Err(DurableError::error(format!("Subagent failed: {:?}", settled.status)));
            };
            let text = answer_text(&api, answer, &context).await?;
            Ok(ToolExecutionResult { output: Some(vec![pi_ai::ContentBlock::text(text)]), ..Default::default() })
        },
    )
    .with_replay(ToolReplay::Safe);
    Extension::new("subagent").tool(tool)
}

// ─── Section 5.1: a task with intent, effect, outcome ────────────────────────

async fn charge(_key: &str) -> DurableResult<String> {
    Ok(String::new())
}

fn payment() -> Task {
    define_task(
        TaskDefinition::new("app.payment", 1, |_| Ok(Json::from(json!({ "phase": "prepare" }))))
            // Intent, effect, outcome.
            .phase("prepare", |_, runtime, context| async move {
                let next =
                    Some(TaskState::Running { checkpoint: Json::from(json!({ "phase": "charge", "key": new_key() })) });
                runtime.commit(move |_, _| async move { Ok(next) }, &context).await
            })
            .phase("charge", |task, runtime, context| async move {
                let key =
                    task.state.checkpoint().and_then(|c| c.get("key")?.as_str().map(str::to_owned)).unwrap_or_default();
                let receipt = charge(&key).await?; // idempotent by key
                let change = move |tx: Tx, current: TaskRecord| async move {
                    let entry = EntryDraft {
                        data: Some(Json::from(json!({ "id": receipt }))),
                        ..EntryDraft::new("app.receipt")
                    };
                    let entry = tx.append_entry(current.conversation_id, entry).await?;
                    let result = Some(Json::from(json!({ "entryId": entry.id })));
                    Ok(Some(TaskState::Terminal { outcome: TaskOutcome::Completed { result } }))
                };
                runtime.commit(change, &context).await
            })
            // The abort handler decides the outcome; returning without one faults the task.
            .abort(|_, runtime, context| async move {
                let aborted = TaskOutcome::Aborted { reason: Some("user".into()), result: None };
                runtime
                    .commit(move |_, _| async move { Ok(Some(TaskState::Terminal { outcome: aborted })) }, &context)
                    .await
            }),
    )
}

// ─── Sequences ───────────────────────────────────────────────────────────────

/// Section 2.2: a tool's commit creates a configured child.
async fn child_in_tool_commit(tx: &Tx, task_id: TaskId, haiku: ModelRef, worktree: &str) -> DurableResult<()> {
    // The child starts as a copy of this conversation's agent: model, extensions, tools, cwd.
    let child = tx.create_conversation(ConversationOwnership::Task { task_id }).await?;
    // A cheaper model, only the read tool, and its own worktree; everything else stays as copied.
    let change = AgentChange {
        model: Change::Set(haiku),
        tools: Change::Set(ToolFilter::List(vec!["read".into()])),
        cwd: Change::Set(worktree.into()),
        ..AgentChange::default()
    };
    configure(tx, child.id, &change).await
}

/// Section 2.2: settings read live, here from a shared value the host updates.
fn live_settings(user: Arc<Mutex<(u64, bool)>>) -> SettingsSource {
    Arc::new(move || {
        let (timeout_ms, auto_compact) = *user.lock().unwrap();
        Ok(HarnessSettings {
            stream: Some(ConversationStreamOptions { timeout_ms: Some(timeout_ms), ..Default::default() }),
            compaction: Some(CompactionOverrides { enabled: Some(auto_compact), ..Default::default() }),
            ..HarnessSettings::default()
        })
    })
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Container {
    image: String,
}

static CONTAINER_DOC: LazyLock<DocToken<Container, LatestConversation>> = LazyLock::new(|| {
    let initial = || Container { image: "node:22".into() };
    define_doc(DocSpec::new("app.container", 1, LatestConversation(LatestFork::Current), initial, |_, _, _| Ok(false)))
        .unwrap()
});

/// Section 2.2: an environment per conversation.
async fn container_harness(
    storage: Arc<dyn Storage>,
    models: Arc<dyn Models>,
    context: &Context,
) -> DurableResult<Harness> {
    let registry = Arc::new(create_registry());
    // Absent: the conversation runs locally. Only conversations with this document run in a container.
    let options = HarnessOptions::new(models, registry).with_env(|target, context| {
        Box::pin(async move {
            let cwd = target.cwd.unwrap_or_else(|| "/work".into());
            match target.read.snapshot(&CONTAINER_DOC, target.conversation_id, &context).await? {
                Some(container) => {
                    container_env(container.get("image").and_then(Json::as_str).unwrap_or(""), &cwd, &context).await
                }
                None => Ok(Some(local_env(&cwd))),
            }
        })
    });
    Harness::open(storage, options, context).await
}

/// Sections 2.2 and 7.1: host setup, tool filters, extension selection, plan mode, and reload.
async fn host(
    storage: Arc<dyn Storage>,
    models: Arc<dyn Models>,
    sonnet: ModelRef,
    context: &Context,
) -> DurableResult<Harness> {
    let registry = Arc::new(create_registry());
    let [context_files, skills, coding, permissions, reviewer, timing, venv] =
        <[Extension; 7]>::try_from(extensions()).unwrap();
    let selected: Vec<Arc<Extension>> =
        [coding_tools(), coding, context_files, skills, permissions].into_iter().map(Arc::new).collect();
    for extension in selected.iter().cloned().chain([Arc::new(reviewer)]) {
        registry.install(extension)?;
    }
    // Reviewer is installed but not selected by default: only conversations that select it get its role and hooks.
    let settings = HarnessSettings { extensions: Some(selected), ..HarnessSettings::default() };
    let options = HarnessOptions::new(models, registry.clone())
        .with_settings(settings)
        .with_env(|target, _| Box::pin(async move { Ok(Some(local_env(&target.cwd.unwrap_or_else(|| ".".into())))) }));
    let harness = Harness::open(storage, options, context).await?;
    // The conversation remembers its model and directory; a restart elsewhere keeps both.
    let agent = AgentChange { model: Change::Set(sonnet), cwd: Change::Set(".".into()), ..AgentChange::default() };
    let root = harness.root(context, CreateOptions { agent: Some(agent), ..CreateOptions::default() }).await?;
    let tools = |filter| AgentChange { tools: filter, ..AgentChange::default() };
    root.configure(tools(Change::Set(ToolFilter::Remove { remove: vec!["edit".into()] })), context).await?;
    // Edit is offered again.
    root.configure(tools(Change::Set(ToolFilter::Remove { remove: vec!["bash".into()] })), context).await?;
    // Every tool of the selected extensions again.
    root.configure(tools(Change::Clear), context).await?;
    registry.install(timing)?;
    registry.install(venv)?; // installed, but not in the default selection
    let add = ExtensionSelection::Edit { add: Some(vec!["venv".into()]), remove: None };
    root.configure(AgentChange { extensions: Change::Set(add), ..AgentChange::default() }, context).await?;
    registry.install(plan_mode())?;
    let id = root.id;
    root.commit(
        move |tx: Tx| async move { tx.doc(&PLAN_MODE_DOC, id).await?.edit(|plan| plan.enabled = true) },
        context,
    )
    .await?;
    registry.install(subagent())?;
    // Same name: conversations selecting skills render v2 at their next request.
    registry.install(Extension::new("skills").section(text_section("skills", render_skills, None)))?;
    Ok(harness)
}

/// Section 7.3: a named subagent's spawn commit.
async fn spawn(tx: &Tx, conversation_id: ConversationId, name: &str) -> DurableResult<()> {
    let options = TaskOptions { background: true, ..TaskOptions::conversation(conversation_id) };
    let anchor = tx.create_task(idle_task("app.anchor"), Json::Null, options).await?;
    // Owned by a task of the parent: starts as a copy of the parent's agent.
    let child = tx.create_conversation(ConversationOwnership::Task { task_id: anchor }).await?;
    let change = AgentChange {
        extensions: Change::Set(ExtensionSelection::Edit { add: None, remove: Some(vec!["subagent-tools".into()]) }),
        instructions: Change::Set(format!("You are the subagent \"{name}\". Answer the main agent's requests.")),
        ..AgentChange::default()
    };
    configure(tx, child.id, &change).await
}

fn render(_: &TranscriptView) {}

/// Section 9.3: a client's transcript over the windowed view.
async fn transcript(conversation: Conversation, context: &Context) -> DurableResult<ConversationWatch> {
    let pages = conversation.clone();
    let load: EntryLoader = Arc::new(move |range: EntryRange, limit: usize, context: Context| {
        let pages = pages.clone();
        futures::FutureExt::boxed(async move { pages.entries(range, limit, None, &context).await })
    });
    let transcript = Arc::new(ConversationTranscript::new(load, None)?);
    let watch = conversation.watch(context, None).await?;
    render(transcript.merge(&watch.value())?.as_ref());
    let merging = transcript.clone();
    watch.start(on_value(move |ValueCall { value, .. }, _| {
        let merged = merging.merge(&value).map(|view| render(&view));
        async move { merged }
    }))?;
    // A marker scrolled into view:
    render(&*transcript.load_earlier(context).await?);
    // Reconnect after a drop:
    conversation.watch(context, transcript.resume()).await
}

/// Section 4: table reads before the first table write; documents stay usable.
async fn table_rules(session: &Session, conversation_id: ConversationId, context: &Context) -> DurableResult<()> {
    let change = move |tx: Tx| async move {
        let conversation = tx.conversation(conversation_id).await?; // table read
        let live = tx.doc(&LIVE_DOC, conversation_id).await?;
        tx.append_entry(conversation_id, EntryDraft::new("app.message")).await?; // first table write
        live.edit(|live| live.generation = None)?; // document mutation remains valid
        let follow = idle_task("app.follow");
        tx.create_task(follow, Json::from(json!({})), TaskOptions::conversation(conversation_id)).await?;
        let _ = conversation;
        Ok(())
    };
    session.commit(change, context).await
}

/// Section 3.4: drafts are revoked after their commit.
async fn revoked_draft(session: &Session, conversation_id: ConversationId, context: &Context) -> DurableResult<()> {
    let escaped =
        session.commit(move |tx: Tx| async move { tx.doc(&LIVE_DOC, conversation_id).await }, context).await?;
    escaped.edit(|live| live.generation = None) // fails: the draft was revoked
}

#[test]
fn compiles_the_specs_usage_examples() {
    // Reached only to name each example; none of them runs.
    let _ = (extensions as fn() -> Vec<Extension>, plan_mode as fn() -> Extension, subagent as fn() -> Extension);
    let _ = (payment as fn() -> Task, live_settings as fn(Arc<Mutex<(u64, bool)>>) -> SettingsSource);
    let _ = (child_in_tool_commit, container_harness, host, spawn, table_rules, revoked_draft);
    let _ = (create_read_tool(ReadToolOptions::default()), create_edit_tool());
}
