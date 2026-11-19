//! Bash calls reach every client: Zed's display-only terminal from metadata, and `rawInput`,
//! `rawOutput`, and text content for clients that show no such terminals, such as Paseo.

use pi_ai::providers::faux::FauxResponse;
use serde_json::{Value, json};

use crate::acp::{setup, start};

/// Run one bash call in a session of a client that sends `client_meta`; returns the session updates
/// of the call.
async fn bash_call(command: &str, client_meta: Value) -> Vec<Value> {
    let setup =
        setup(vec![FauxResponse::tool_call("b1", "bash", json!({"command": command})), FauxResponse::text("Done.")]);
    let work = setup.root.path().join("work");
    let mut client = start(&setup);
    let capabilities = json!({"terminal": true, "_meta": client_meta});
    client.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": capabilities})).await;
    let created = client.request("session/new", json!({"cwd": work, "mcpServers": []})).await;
    let id = created["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{created}")).to_string();
    client.request("session/set_mode", json!({"sessionId": id, "modeId": "bypass"})).await;
    let prompt = json!({"sessionId": id, "prompt": [{"type": "text", "text": "run it"}]});
    let response = client.request("session/prompt", prompt).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    client.updates(&id).into_iter().filter(|update| update["toolCallId"] == "b1").collect()
}

fn text_of(content: &Value) -> String {
    let blocks = content.as_array().into_iter().flatten();
    blocks.filter(|block| block["type"] == "content").filter_map(|block| block["content"]["text"].as_str()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn zed_keeps_its_terminal_and_other_clients_get_the_output_as_text() {
    let zed = bash_call("echo bash-output-ok", json!({"terminal_output": true, "terminal-auth": true})).await;
    let paseo = bash_call("echo bash-output-ok", json!({})).await;
    for (client, updates) in [("zed", &zed), ("paseo", &paseo)] {
        let created = updates.iter().find(|update| update["sessionUpdate"] == "tool_call").expect("tool_call");
        assert_eq!(created["content"], json!([{"type": "terminal", "terminalId": "b1"}]), "{client}");
        assert!(created["_meta"]["terminal_info"].is_object(), "{client}");
        let started = updates.iter().find(|update| update["status"] == "in_progress").expect("started");
        assert_eq!(started["rawInput"]["command"], "echo bash-output-ok", "{client}");
        let done = updates.iter().find(|update| update["status"] == "completed").expect("completed");
        assert_eq!(done["rawOutput"]["exitCode"], 0, "{client}");
        assert!(done["rawOutput"]["output"].as_str().unwrap().contains("bash-output-ok"), "{client}: {done}");
        assert_eq!(done["_meta"]["terminal_exit"]["exit_code"], 0, "{client}");
    }
    assert!(zed.iter().all(|update| text_of(&update["content"]).is_empty()), "no text next to Zed's terminal");
    let done = paseo.iter().find(|update| update["status"] == "completed").unwrap();
    assert_eq!(done["content"][0], json!({"type": "terminal", "terminalId": "b1"}));
    assert!(text_of(&done["content"]).contains("bash-output-ok"), "{done}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_command_reports_its_exit_code() {
    let updates = bash_call("echo partial; exit 3", json!({})).await;
    let failed = updates.iter().find(|update| update["status"] == "failed").expect("failed");
    assert_eq!(failed["rawOutput"]["exitCode"], 3, "{failed}");
    assert_eq!(failed["_meta"]["terminal_exit"]["exit_code"], 3, "{failed}");
    assert!(text_of(&failed["content"]).contains("partial"), "{failed}");
}
