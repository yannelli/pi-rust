//! Session commands: names, entries, tree, stats, fork, clone, switch, new, bash, compaction,
//! export, and discoverable commands (upstream `rpc.test.ts` session cases).

use pi_ai::providers::faux::FauxResponse;
use serde_json::{Value, json};

use crate::harness::Options;
use crate::support::{Rpc, start};

async fn prompt(rpc: &mut Rpc, id: &str, message: &str, settled: usize) {
    let data = rpc.data(json!({"id": id, "type": "prompt", "message": message})).await;
    assert_eq!(data["disposition"], "started");
    rpc.settled(settled).await;
}

fn entry_ids(entries: &Value) -> Vec<String> {
    entries.as_array().unwrap().iter().map(|entry| entry["id"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn names_sessions_and_reads_entries_trees_and_stats() {
    let mut rpc = start(Options { responses: vec![FauxResponse::text("ok")], ..Options::default() }).await;
    assert!(rpc.data(json!({"id": "s0", "type": "get_state"})).await.get("sessionName").is_none());
    assert_eq!(rpc.data(json!({"id": "t0", "type": "get_last_assistant_text"})).await, json!({"text": null}));
    prompt(&mut rpc, "p", "Reply with just ok", 1).await;

    let named = rpc.request(json!({"id": "n", "type": "set_session_name", "name": "  my-test-session "})).await;
    assert_eq!(named, json!({"id": "n", "type": "response", "command": "set_session_name", "success": true}));
    assert_eq!(rpc.data(json!({"id": "s1", "type": "get_state"})).await["sessionName"], "my-test-session");
    rpc.wait_for(|record| record["type"] == "session_info_changed" && record["name"] == "my-test-session").await;
    let empty = rpc.request(json!({"id": "e", "type": "set_session_name", "name": "   "})).await;
    assert_eq!(empty["error"], "Session name cannot be empty");

    let all = rpc.data(json!({"id": "g", "type": "get_entries"})).await;
    let ids = entry_ids(&all["entries"]);
    assert!(ids.len() >= 3);
    assert_eq!(all["leafId"].as_str().unwrap(), ids.last().unwrap());
    assert!(all["entries"].as_array().unwrap().iter().all(|entry| entry["type"] != "session"), "no header");
    let since = rpc.data(json!({"id": "g2", "type": "get_entries", "since": ids[0]})).await;
    assert_eq!(entry_ids(&since["entries"]), ids[1..]);
    let missing = rpc.request(json!({"id": "g3", "type": "get_entries", "since": "nonexistent-id"})).await;
    assert_eq!(missing["error"], "Entry not found: nonexistent-id");

    let tree = rpc.data(json!({"id": "tr", "type": "get_tree"})).await;
    assert_eq!(tree["leafId"], all["leafId"]);
    let mut chain = Vec::new();
    let mut nodes = tree["tree"].as_array().unwrap().clone();
    while nodes.len() == 1 {
        chain.push(nodes[0]["entry"]["id"].as_str().unwrap().to_string());
        nodes = nodes[0]["children"].as_array().unwrap().clone();
    }
    assert!(nodes.is_empty());
    assert_eq!(chain, ids);

    let stats = rpc.data(json!({"id": "st", "type": "get_session_stats"})).await;
    assert_eq!((stats["userMessages"].clone(), stats["assistantMessages"].clone()), (json!(1), json!(1)));
    assert!(stats["sessionFile"].is_string() && stats["sessionId"].is_string() && stats["tokens"]["total"].is_u64());
    assert_eq!(stats["contextUsage"]["contextWindow"], 128_000);

    let file = rpc.runtime.session().session_file().unwrap();
    let saved = std::fs::read_to_string(file).unwrap();
    assert!(saved.lines().any(|line| line.contains("\"session_info\"") && line.contains("my-test-session")));
    rpc.finish().await;
}

#[tokio::test]
async fn forks_clones_switches_and_starts_new_sessions() {
    let responses = vec![FauxResponse::text("first answer"), FauxResponse::text("second answer")];
    let mut rpc = start(Options { responses, ..Options::default() }).await;
    prompt(&mut rpc, "p1", "first question", 1).await;
    prompt(&mut rpc, "p2", "second question", 2).await;
    let original = rpc.data(json!({"id": "s0", "type": "get_state"})).await["sessionFile"].clone();

    let forkable = rpc.data(json!({"id": "fm", "type": "get_fork_messages"})).await["messages"].clone();
    let texts: Vec<&str> =
        forkable.as_array().unwrap().iter().map(|message| message["text"].as_str().unwrap()).collect();
    assert_eq!(texts, ["first question", "second question"]);
    let second = forkable[1]["entryId"].as_str().unwrap().to_string();
    let forked = rpc.data(json!({"id": "f", "type": "fork", "entryId": second})).await;
    assert_eq!(forked, json!({"text": "second question", "cancelled": false}));
    let state = rpc.data(json!({"id": "s1", "type": "get_state"})).await;
    assert_ne!(state["sessionFile"], original);
    assert_eq!(state["messageCount"], 3);
    rpc.data(json!({"id": "n1", "type": "set_session_name", "name": "forked"})).await;
    rpc.wait_for(|record| record["type"] == "session_info_changed" && record["name"] == "forked").await;

    let bad = rpc.request(json!({"id": "f2", "type": "fork", "entryId": "missing"})).await;
    assert_eq!(bad["error"], "Invalid entry ID for forking");
    let forked_file = state["sessionFile"].clone();
    assert_eq!(rpc.data(json!({"id": "c", "type": "clone"})).await, json!({"cancelled": false}));
    let cloned = rpc.data(json!({"id": "s2", "type": "get_state"})).await;
    assert_ne!(cloned["sessionFile"], forked_file);
    assert_eq!(cloned["messageCount"], 3);

    assert_eq!(rpc.data(json!({"id": "new", "type": "new_session"})).await, json!({"cancelled": false}));
    assert_eq!(rpc.data(json!({"id": "s3", "type": "get_state"})).await["messageCount"], 0);
    let parent = original.as_str().unwrap();
    rpc.data(json!({"id": "new2", "type": "new_session", "parentSession": parent})).await;
    let header = rpc.runtime.session().session_manager().header_raw().clone();
    assert_eq!(header["parentSession"], parent);

    let switched = rpc.data(json!({"id": "sw", "type": "switch_session", "sessionPath": parent})).await;
    assert_eq!(switched, json!({"cancelled": false}));
    let state = rpc.data(json!({"id": "s4", "type": "get_state"})).await;
    assert_eq!((state["sessionFile"].clone(), state["messageCount"].clone()), (original.clone(), json!(5)));
    rpc.finish().await;
}

#[tokio::test]
async fn bash_streams_updates_records_results_and_aborts() {
    let mut rpc = start(Options { responses: vec![FauxResponse::text("seen")], ..Options::default() }).await;
    let result = rpc.data(json!({"id": "b1", "type": "bash", "command": "echo hello; echo world"})).await;
    assert_eq!(result, json!({"output": "hello\nworld\n", "exitCode": 0, "cancelled": false, "truncated": false}));
    let updates: String = rpc
        .records_of("bash_execution_update")
        .iter()
        .inspect(|update| assert_eq!(update["id"], "b1"))
        .map(|update| update["delta"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(updates, "hello\nworld\n");
    let hidden = rpc.data(json!({"id": "b2", "type": "bash", "command": "exit 3", "excludeFromContext": true})).await;
    assert_eq!(hidden["exitCode"], 3);

    rpc.send(json!({"id": "long", "type": "bash", "command": "echo start; sleep 30"})).await;
    rpc.wait_for(|record| record["type"] == "bash_execution_update" && record["id"] == "long").await;
    let aborted = rpc.request(json!({"id": "ab", "type": "abort_bash"})).await;
    assert_eq!(aborted, json!({"id": "ab", "type": "response", "command": "abort_bash", "success": true}));
    let cancelled = rpc.response("long").await["data"].clone();
    assert_eq!(cancelled, json!({"output": "start\n", "cancelled": true, "truncated": false}));

    let messages = rpc.data(json!({"id": "m", "type": "get_messages"})).await["messages"].clone();
    let bash: Vec<&Value> = messages.as_array().unwrap().iter().filter(|m| m["role"] == "bashExecution").collect();
    assert_eq!(bash.len(), 3);
    assert_eq!(
        (bash[0]["output"].clone(), bash[1]["excludeFromContext"].clone()),
        (json!("hello\nworld\n"), json!(true))
    );
    prompt(&mut rpc, "p", "What did it print?", 1).await;
    let request = serde_json::to_string(&rpc.h.requests()[0].messages).unwrap();
    assert!(request.contains("Ran `echo hello; echo world`") && !request.contains("exit 3"), "{request}");
    rpc.finish().await;
}

#[tokio::test]
async fn compacts_and_exports_html() {
    let responses = vec![
        FauxResponse::text("first"),
        FauxResponse::text("second"),
        FauxResponse::text("## Goal\nSummarized work."),
    ];
    let settings = json!({"compaction": {"keepRecentTokens": 6}});
    let mut rpc = start(Options { responses, settings, ..Options::default() }).await;
    prompt(&mut rpc, "p", "Say hello", 1).await;
    prompt(&mut rpc, "p2", "Say it again", 2).await;
    let before = entry_ids(&rpc.data(json!({"id": "e0", "type": "get_entries"})).await["entries"]);

    let compacted = rpc.data(json!({"id": "c", "type": "compact", "customInstructions": "Focus on code"})).await;
    assert!(compacted["summary"].as_str().unwrap().contains("Summarized work."), "{compacted}");
    assert!(compacted["tokensBefore"].as_u64().unwrap() > 0 && compacted["firstKeptEntryId"].is_string());
    assert!(compacted["usage"]["totalTokens"].is_u64());
    let end = rpc.wait_for(|record| record["type"] == "compaction_end").await;
    assert_eq!((end["reason"].clone(), end["aborted"].clone()), (json!("manual"), json!(false)));
    let after = rpc.data(json!({"id": "e1", "type": "get_entries"})).await["entries"].clone();
    assert_eq!(entry_ids(&after)[..before.len()], before[..], "entries are append-only");
    assert!(after.as_array().unwrap().iter().any(|entry| entry["type"] == "compaction"));

    let target = rpc.h.cwd.join("out/session.html");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    let exported = rpc.data(json!({"id": "x", "type": "export_html", "outputPath": target})).await;
    assert_eq!(exported["path"], target.display().to_string());
    assert!(std::fs::read_to_string(&target).unwrap().contains("<html"));
    rpc.finish().await;
}

#[tokio::test]
async fn lists_prompt_templates_and_skills_as_commands() {
    let files = vec![
        (".pi/prompts/fix-tests.md", "---\ndescription: Fix failing tests\n---\nFix the failing tests in $1."),
        (".pi/skills/lint/SKILL.md", "---\nname: lint\ndescription: Lint the code\n---\nRun the linter."),
    ];
    let mut rpc = start(Options { files, responses: vec![FauxResponse::text("done")], ..Options::default() }).await;
    let commands = rpc.data(json!({"id": "c", "type": "get_commands"})).await["commands"].clone();
    let fix = commands.as_array().unwrap().iter().find(|command| command["name"] == "fix-tests").unwrap();
    assert_eq!((fix["description"].clone(), fix["source"].clone()), (json!("Fix failing tests"), json!("prompt")));
    let info = &fix["sourceInfo"];
    assert!(info["path"].as_str().unwrap().ends_with("fix-tests.md"));
    assert_eq!((info["scope"].clone(), info["origin"].clone()), (json!("project"), json!("top-level")));
    let skill = commands.as_array().unwrap().iter().find(|command| command["name"] == "skill:lint").unwrap();
    assert_eq!((skill["source"].clone(), skill["description"].clone()), (json!("skill"), json!("Lint the code")));

    prompt(&mut rpc, "p", "/fix-tests parser", 1).await;
    let request = serde_json::to_string(&rpc.h.requests()[0].messages).unwrap();
    assert!(request.contains("Fix the failing tests in parser."), "{request}");
    rpc.finish().await;
}
