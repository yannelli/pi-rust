//! Port of upstream `test/harness-prompt.test.ts` (system prompt and tool loadout preparation).

mod support;

use std::sync::{Arc, Mutex};

use pi_ai::transcript::current_tools;
use pi_ai::{Message, SystemMessage, Tool, UserContent, UserMessage};
use pi_chord::{AbortController, ChordError, with_abort_signal};
use pi_durable::harness::agent::resolve_agent;
use pi_durable::harness::prompt::{
    Sections, plan_system_entries, render_sections, replay_sections, to_tool_declaration,
};
use pi_durable::harness::*;
use pi_durable::session::{SessionOptions, Tx, create_session};
use pi_durable::storage::MemoryStorage;
use pi_durable::{DraftHead, DurableError, EntryDraft, EntryId};
use serde_json::{Value, json};
use support::context;
use support::task_support::open_tasks;

fn user(text: &str) -> Message {
    Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp: 1 })
}

fn sections(pairs: &[(&str, &str)]) -> Sections {
    pairs.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect()
}

fn system_of(draft: &EntryDraft) -> SystemMessage {
    match draft.model.as_ref().and_then(|model| model.first()) {
        Some(Message::System(system)) => system.clone(),
        other => panic!("expected a system message, got {other:?}"),
    }
}

async fn append_all(conversation: &Conversation, drafts: Vec<EntryDraft>) {
    let id = conversation.id;
    let commit = move |tx: Tx| async move {
        for draft in drafts {
            tx.append_entry(id, draft).await?;
        }
        Ok(())
    };
    conversation.commit(commit, &context()).await.unwrap();
}

/// Plan against the current context, append the plan, and check that replay then yields `wanted` in order. Each
/// planned entry reads as JSON text `{ sections, omit? }`, so key order is compared too.
async fn apply(conversation: &Conversation, wanted: &[(&str, &str)]) -> Vec<String> {
    let view = conversation.context(&context(), None).await.unwrap();
    let drafts = plan_system_entries(&view, &sections(wanted), &[], 7.0);
    append_all(conversation, drafts.clone()).await;
    let replayed = replay_sections(&conversation.context(&context(), None).await.unwrap().messages);
    assert_eq!(replayed, sections(wanted));
    drafts
        .iter()
        .map(|draft| {
            let message = system_of(draft);
            assert_eq!((message.content.clone(), message.timestamp), (UserContent::Text(String::new()), 7));
            let mut planned = json!({ "sections": message.sections.unwrap() });
            if let Some(edits) = &draft.edits {
                let omit: Vec<EntryId> = edits.iter().map(|edit| edit.target).collect();
                assert!(edits.iter().all(|edit| serde_json::to_value(edit).unwrap()["action"] == "omit"));
                planned["omit"] = json!(omit);
            }
            planned.to_string()
        })
        .collect()
}

fn planned(list: &[Value]) -> Vec<String> {
    list.iter().map(Value::to_string).collect()
}

async fn root() -> Conversation {
    let harness = open_tasks(Arc::new(MemoryStorage::new()), Vec::new()).await.harness;
    harness.root(&context(), CreateOptions::default()).await.unwrap()
}

async fn last_system_id(conversation: &Conversation) -> EntryId {
    let page = conversation.entries(EntryRange::default(), 100, None, &context()).await.unwrap();
    page.items.iter().find(|entry| entry.kind == "pi.system").unwrap().id
}

async fn marker(conversation: &Conversation, head: DraftHead) -> EntryId {
    let id = conversation.id;
    let commit = move |tx: Tx| async move {
        let draft = EntryDraft { head: Some(head), model: Some(vec![user("summary")]), ..EntryDraft::new("summary") };
        Ok(tx.append_entry(id, draft).await?.id)
    };
    conversation.commit(commit, &context()).await.unwrap()
}

fn text_section(key: &str, text: Option<&str>, tag: Option<bool>) -> Arc<PromptSection> {
    let text = text.map(str::to_owned);
    section(key, move |_, _| std::future::ready(Ok(text.clone())), tag)
}

fn failing(key: &str, message: &str) -> Arc<PromptSection> {
    let message = message.to_owned();
    section(key, move |_, _| std::future::ready(Err(DurableError::error(message.clone()))), None)
}

fn input(agent: Agent) -> PromptInput {
    let read = create_session(Arc::new(MemoryStorage::new()), SessionOptions::default());
    PromptInput { conversation_id: pi_durable::ROOT_CONVERSATION_ID, agent, env: None, shown: Default::default(), read }
}

fn empty_agent() -> Agent {
    let registry = create_registry();
    resolve_agent(None, &registry.snapshot(), &resolve_settings(None), &|error| panic!("{error}"))
}

#[tokio::test]
async fn renders_sections_in_order_with_tags_omissions_wrappers_and_failures() {
    let registry = create_registry();
    let install = |section: Arc<PromptSection>| {
        registry.install(Extension::new(format!("section:{}", section.key)).section(section)).unwrap();
    };
    install(text_section("preamble", Some("You are helpful."), Some(false)));
    install(text_section("cwd", Some("/repo"), None));
    install(text_section("skipped", None, None));
    install(failing("failing", "render failed"));
    install(failing("new-failing", "also failed"));
    let wrap = wrap_section("cwd", |inner| {
        let render = inner.render.clone();
        Ok(section(
            inner.key.clone(),
            move |input, context| {
                let rendered = render(input, context);
                async move { Ok(rendered.await?.map(|text| format!("{text} (git)"))) }
            },
            inner.tag,
        ))
    });
    registry.install(Extension::new("git").wrap(wrap)).unwrap();
    let reports = Mutex::new(Vec::new());
    let shown = sections(&[("failing", "<failing>\nold\n</failing>"), ("cwd", "stale")]);
    let agent = resolve_agent(None, &registry.snapshot(), &resolve_settings(None), &|error| panic!("{error}"));
    let report = |error: &DurableError| reports.lock().unwrap().push(error.to_string());
    let rendered = render_sections(&agent.sections, &input(agent.clone()), &shown, &report, &context()).await.unwrap();
    let expected = [
        ("preamble", "You are helpful."),
        ("cwd", "<cwd>\n/repo (git)\n</cwd>"),
        ("failing", "<failing>\nold\n</failing>"),
    ];
    assert_eq!(rendered, sections(&expected));
    assert_eq!(*reports.lock().unwrap(), ["render failed", "also failed"]);
}

#[tokio::test]
async fn propagates_section_errors_after_cancellation() {
    let controller = AbortController::new();
    controller.abort(Some(ChordError::error("cancelled")));
    let cancelled = with_abort_signal(&controller.signal(), &context());
    let sections = [failing("a", "cancelled")];
    let error = render_sections(&sections, &input(empty_agent()), &Vec::new(), &|_| {}, &cancelled).await.unwrap_err();
    assert!(error.to_string().contains("cancelled"));
}

#[test]
fn replays_sections_in_place_deletes_on_null_and_appends_re_additions() {
    let system = |sections: Value| {
        let sections = sections.as_object().cloned();
        Message::System(SystemMessage {
            content: UserContent::Text(String::new()),
            sections,
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
        })
    };
    let shown = replay_sections(&[
        system(json!({ "a": "1", "b": "2", "c": "3" })),
        user("x"),
        system(json!({ "b": "20", "a": null })),
        system(json!({ "a": "10" })),
    ]);
    assert_eq!(shown, sections(&[("b", "20"), ("c", "3"), ("a", "10")]));
}

#[tokio::test]
async fn emits_minimal_value_patches_removals_and_additions() {
    let conversation = root().await;
    let all = [("a", "1"), ("b", "2"), ("c", "3")];
    assert_eq!(apply(&conversation, &all).await, planned(&[json!({ "sections": { "a": "1", "b": "2", "c": "3" } })]));
    let changed = [("a", "1"), ("b", "20"), ("c", "3"), ("d", "4")];
    assert_eq!(apply(&conversation, &changed).await, planned(&[json!({ "sections": { "b": "20", "d": "4" } })]));
    let removed = [("a", "1"), ("c", "3"), ("d", "4")];
    assert_eq!(apply(&conversation, &removed).await, planned(&[json!({ "sections": { "b": null } })]));
    assert_eq!(apply(&conversation, &removed).await, Vec::<String>::new());
    let cleared = planned(&[json!({ "sections": { "a": null, "c": null, "d": null } })]);
    assert_eq!(apply(&conversation, &[]).await, cleared);
}

#[tokio::test]
async fn rewrites_order_only_changes_and_re_additions_as_two_entries() {
    let conversation = root().await;
    apply(&conversation, &[("a", "1"), ("b", "2")]).await;
    let expected =
        planned(&[json!({ "sections": { "a": null, "b": null } }), json!({ "sections": { "b": "2", "a": "1" } })]);
    assert_eq!(apply(&conversation, &[("b", "2"), ("a", "1")]).await, expected);
    let readded = root().await;
    apply(&readded, &[("a", "1"), ("b", "2"), ("c", "3")]).await;
    assert_eq!(apply(&readded, &[("a", "1"), ("c", "3")]).await, planned(&[json!({ "sections": { "b": null } })]));
    // Patching would append `b` after `c`.
    let expected = planned(&[
        json!({ "sections": { "a": null, "c": null } }),
        json!({ "sections": { "a": "1", "b": "2", "c": "3" } }),
    ]);
    assert_eq!(apply(&readded, &[("a", "1"), ("b", "2"), ("c", "3")]).await, expected);
}

#[tokio::test]
async fn rebaselines_after_a_head_marker_omitting_kept_deltas_on_both_sides_of_it() {
    let conversation = root().await;
    apply(&conversation, &[("a", "1"), ("b", "2")]).await;
    let id = conversation.id;
    let hi = move |tx: Tx| async move {
        tx.append_entry(id, EntryDraft { model: Some(vec![user("hi")]), ..EntryDraft::new("pi.user") })
            .await
            .map(|_| ())
    };
    conversation.commit(hi, &context()).await.unwrap();
    apply(&conversation, &[("a", "1"), ("b", "20")]).await;
    let delta = last_system_id(&conversation).await;
    // The head keeps the delta but cuts its baseline: replay alone would show only `b`.
    marker(&conversation, DraftHead::Entry(delta)).await;
    let expected = planned(&[json!({ "sections": { "a": "1", "b": "20" }, "omit": [delta] })]);
    assert_eq!(apply(&conversation, &[("a", "1"), ("b", "20")]).await, expected);
    let baseline = last_system_id(&conversation).await;
    // A system entry follows the marker now, so later changes are ordinary patches.
    assert_eq!(
        apply(&conversation, &[("a", "1"), ("b", "21")]).await,
        planned(&[json!({ "sections": { "b": "21" } })])
    );
    let after = last_system_id(&conversation).await;
    // A second marker keeps deltas from both sides of the first one.
    marker(&conversation, DraftHead::Entry(delta)).await;
    let expected = planned(&[json!({ "sections": { "a": "1", "b": "21" }, "omit": [delta, baseline, after] })]);
    assert_eq!(apply(&conversation, &[("a", "1"), ("b", "21")]).await, expected);
}

#[tokio::test]
async fn writes_a_complete_post_head_baseline_even_when_replay_already_matches() {
    let conversation = root().await;
    apply(&conversation, &[("a", "1")]).await;
    let baseline = last_system_id(&conversation).await;
    marker(&conversation, DraftHead::Entry(baseline)).await;
    let expected = planned(&[json!({ "sections": { "a": "1" }, "omit": [baseline] })]);
    assert_eq!(apply(&conversation, &[("a", "1")]).await, expected);
    marker(&conversation, DraftHead::SelfEntry).await;
    assert_eq!(apply(&conversation, &[]).await, planned(&[json!({ "sections": {} })]));
    assert_eq!(apply(&conversation, &[]).await, Vec::<String>::new());
}

fn declaration(name: &str, description: &str) -> Tool {
    Tool {
        name: name.into(),
        description: description.into(),
        parameters: json!({ "type": "object", "properties": {} }),
        constrained_sampling: None,
    }
}

/// Plan tools only, append the plan, check that replay offers `tools` in order, and return each message's changes.
async fn apply_tools(conversation: &Conversation, tools: &[Tool], wanted: &[(&str, &str)]) -> Vec<String> {
    let view = conversation.context(&context(), None).await.unwrap();
    let drafts = plan_system_entries(&view, &sections(wanted), tools, 7.0);
    append_all(conversation, drafts.clone()).await;
    let offered = current_tools(&conversation.context(&context(), None).await.unwrap().messages);
    assert_eq!(offered, tools.iter().map(to_tool_declaration).collect::<Vec<_>>());
    drafts
        .iter()
        .map(|draft| {
            let message = system_of(draft);
            let mut changes = json!({});
            if let Some(removed) = message.tools_removed {
                changes["removed"] = json!(removed.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>());
            }
            if let Some(added) = message.tools_added {
                changes["added"] = json!(added.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>());
            }
            if let Some(sections) = message.sections {
                changes["sections"] = Value::Object(sections);
            }
            changes.to_string()
        })
        .collect()
}

#[tokio::test]
async fn adds_removes_replaces_changed_declarations_and_rewrites_the_order_when_needed() {
    let conversation = root().await;
    let (a, b, c) = (declaration("a", "a"), declaration("b", "b"), declaration("c", "c"));
    let tools = |list: &[&Tool]| list.iter().map(|tool| (*tool).clone()).collect::<Vec<_>>();
    assert_eq!(apply_tools(&conversation, &tools(&[&a, &b]), &[]).await, planned(&[json!({ "added": ["a", "b"] })]));
    assert_eq!(apply_tools(&conversation, &tools(&[&a, &b]), &[]).await, Vec::<String>::new());
    assert_eq!(apply_tools(&conversation, &tools(&[&a, &b, &c]), &[]).await, planned(&[json!({ "added": ["c"] })]));
    assert_eq!(apply_tools(&conversation, &tools(&[&a, &c]), &[]).await, planned(&[json!({ "removed": ["b"] })]));
    // A changed declaration at the end is removed and re-added in place.
    let c2 = declaration("c", "changed");
    let expected = planned(&[json!({ "removed": ["c"], "added": ["c"] })]);
    assert_eq!(apply_tools(&conversation, &tools(&[&a, &c2]), &[]).await, expected);
    // A changed declaration in the middle would move to the end, so the whole order is rewritten.
    let a2 = declaration("a", "changed");
    let expected = planned(&[json!({ "removed": ["a", "c"], "added": ["a", "c"] })]);
    assert_eq!(apply_tools(&conversation, &tools(&[&a2, &c2]), &[]).await, expected);
    // Order-only change.
    let expected = planned(&[json!({ "removed": ["a", "c"], "added": ["c", "a"] })]);
    assert_eq!(apply_tools(&conversation, &tools(&[&c2, &a2]), &[]).await, expected);
    assert_eq!(apply_tools(&conversation, &[], &[]).await, planned(&[json!({ "removed": ["c", "a"] })]));
}

#[tokio::test]
async fn puts_tool_changes_on_the_last_section_entry_and_re_declares_every_tool_after_a_head_cut() {
    let conversation = root().await;
    let (a, b) = (declaration("a", "a"), declaration("b", "b"));
    let expected = planned(&[json!({ "added": ["a"], "sections": { "x": "1", "y": "2" } })]);
    assert_eq!(apply_tools(&conversation, std::slice::from_ref(&a), &[("x", "1"), ("y", "2")]).await, expected);
    // Section order changes need two entries; the tool change rides on the second.
    let expected = planned(&[
        json!({ "sections": { "x": null, "y": null } }),
        json!({ "added": ["b"], "sections": { "y": "2", "x": "1" } }),
    ]);
    assert_eq!(apply_tools(&conversation, &[a.clone(), b.clone()], &[("y", "2"), ("x", "1")]).await, expected);
    marker(&conversation, DraftHead::SelfEntry).await;
    let expected = planned(&[json!({ "added": ["a", "b"], "sections": { "y": "2", "x": "1" } })]);
    assert_eq!(apply_tools(&conversation, &[a, b], &[("y", "2"), ("x", "1")]).await, expected);
}
