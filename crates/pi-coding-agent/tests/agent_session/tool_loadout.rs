//! The loadout a resumed session or `/tree` navigation restores from the branch (upstream
//! `default-tools-setting.test.ts`, "restores the loadout the transcript records" and after).

use std::sync::Arc;

use pi_ai::Message;
use pi_coding_agent::agent_session::{AgentSession, NavigateTreeOptions, NoTools};
use pi_coding_agent::session::SessionManager;
use serde_json::{Value, json};

use crate::harness::{Harness, open_session_with};
use crate::tool_registry::{Select, apply, session_with, sorted, tool};

fn references(names: &[&str]) -> Vec<Value> {
    names
        .iter()
        .map(|name| json!({"name": name, "description": name, "parameters": {"type": "object", "properties": {}}}))
        .collect()
}

/// Append a system message that adds and removes tools, as a run records loadout changes.
fn record_change(manager: &mut SessionManager, added: &[&str], removed: &[&str]) -> String {
    let mut system = json!({"role": "system", "content": "", "timestamp": 1});
    if !added.is_empty() {
        system["toolsAdded"] = json!(references(added));
    }
    if !removed.is_empty() {
        system["toolsRemoved"] = json!(references(removed));
    }
    manager.append_message(&serde_json::from_value::<Message>(system).unwrap()).unwrap()
}

fn user_message(manager: &mut SessionManager, text: &str) -> String {
    let message = json!({"role": "user", "content": text, "timestamp": 1});
    manager.append_message(&serde_json::from_value::<Message>(message).unwrap()).unwrap()
}

/// A session whose transcript records the loadout `names`, as a run records it.
fn recorded(h: &Harness, names: &[&str]) -> SessionManager {
    let mut manager = SessionManager::in_memory(&h.cwd);
    record_change(&mut manager, names, &[]);
    manager
}

/// A session that recorded `read`, `grep`, and `inactive_tool`, and deactivated `active_tool`.
fn changed(h: &Harness) -> SessionManager {
    let mut manager = recorded(h, &["read", "grep", "active_tool"]);
    record_change(&mut manager, &["inactive_tool"], &["active_tool"]);
    manager
}

async fn open(manager: impl FnOnce(&Harness) -> SessionManager, select: Select) -> (Harness, Arc<AgentSession>) {
    let h = session_with(Select::default()).await;
    let manager = manager(&h);
    let custom = vec![tool("inactive_tool", false), tool("active_tool", true)];
    let select = Select { custom: [custom, select.custom].concat(), ..select };
    let session = open_session_with(&h.services, manager, |create| apply(create, select)).await;
    (h, session)
}

async fn resume(manager: impl FnOnce(&Harness) -> SessionManager, select: Select) -> Vec<String> {
    sorted(&open(manager, select).await.1)
}

#[tokio::test]
async fn a_resumed_session_restores_the_loadout_its_transcript_records() {
    assert_eq!(resume(changed, Select::default()).await, ["grep", "inactive_tool", "read"]);
    let excluded = Select { exclude: vec!["inactive_tool"], ..Select::default() };
    assert_eq!(resume(changed, excluded).await, ["grep", "read"]);
    let modified = Select { tools: Some(vec!["-grep", "+write"]), ..Select::default() };
    assert_eq!(resume(changed, modified).await, ["inactive_tool", "read", "write"]);
}

#[tokio::test]
async fn activates_extension_tools_the_transcript_never_added_or_removed() {
    // Built-in tools and tools registered inactive stay inactive.
    let session = resume(|h| recorded(h, &["read", "grep"]), Select::default()).await;
    assert_eq!(session, ["active_tool", "grep", "read"]);
}

#[tokio::test]
async fn keeps_tools_removed_before_a_compaction_inactive() {
    let compacted = |h: &Harness| {
        let mut manager = changed(h);
        let kept = user_message(&mut manager, "hi");
        manager.append_compaction("summary", &kept, 1000).unwrap();
        manager
    };
    assert_eq!(resume(compacted, Select::default()).await, ["grep", "inactive_tool", "read"]);
}

#[tokio::test]
async fn restores_the_target_branch_loadout_on_tree_navigation() {
    // The target branch never mentions active_tool, so it is active there. It removed grep.
    let target = std::sync::Mutex::new(String::new());
    let branched = |h: &Harness| {
        let mut manager = recorded(h, &["read", "grep"]);
        record_change(&mut manager, &[], &["grep"]);
        *target.lock().unwrap() = user_message(&mut manager, "first");
        record_change(&mut manager, &["grep"], &["active_tool"]);
        manager
    };
    let (_h, session) = open(branched, Select::default()).await;
    assert_eq!(sorted(&session), ["grep", "read"]);
    let target = target.lock().unwrap().clone();
    session.navigate_tree(&target, NavigateTreeOptions::default()).await.unwrap();
    assert_eq!(sorted(&session), ["active_tool", "read"]);
}

#[tokio::test]
async fn keeps_registered_tools_outside_the_tools_allowlist_inactive_on_tree_navigation() {
    // An allowlist without mcp__ entries keeps MCP tools registered but inactive.
    let target = std::sync::Mutex::new(String::new());
    let branched = |h: &Harness| {
        let mut manager = recorded(h, &["read"]);
        *target.lock().unwrap() = user_message(&mut manager, "first");
        manager
    };
    let select =
        Select { tools: Some(vec!["read"]), custom: vec![tool("mcp__server__lookup", true)], ..Select::default() };
    let (_h, session) = open(branched, select).await;
    assert_eq!(session.active_tool_names(), ["read"]);
    let target = target.lock().unwrap().clone();
    session.navigate_tree(&target, NavigateTreeOptions::default()).await.unwrap();
    assert_eq!(session.active_tool_names(), ["read"]);
}

#[tokio::test]
async fn explicit_tool_options_replace_the_recorded_loadout() {
    let allowlisted = Select { tools: Some(vec!["read", "write"]), ..Select::default() };
    assert_eq!(resume(changed, allowlisted).await, ["read", "write"]);
    let builtinless = Select { no_tools: Some(NoTools::Builtin), ..Select::default() };
    assert_eq!(resume(changed, builtinless).await, ["active_tool"]);
}
