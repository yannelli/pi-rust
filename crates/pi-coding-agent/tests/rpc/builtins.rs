//! Built-in MCP in RPC mode: `/mcp` is a discoverable extension command handled without a run,
//! and its notices reach the client as `notify` requests.

use serde_json::{Value, json};

use crate::harness::{Options, wait_until};
use crate::mcp_fake::{Fake, FakeOptions};
use crate::support::start_with;

#[tokio::test]
async fn lists_and_runs_the_mcp_command() {
    let fake = Fake::new(FakeOptions::default());
    let mcp = fake.session_options("docs", json!({"url": "http://unused.invalid"}), json!({}));
    let mut rpc = start_with(Options::default(), Default::default(), |_, app| app.mcp = mcp).await;
    let session = rpc.runtime.session();
    wait_until(|| session.all_tools().iter().any(|tool| tool.name.starts_with("mcp__docs__"))).await;

    let commands = rpc.data(json!({"id": "c", "type": "get_commands"})).await["commands"].clone();
    let mcp = &commands[0];
    assert_eq!((mcp["name"].clone(), mcp["source"].clone()), (json!("mcp"), json!("extension")));
    assert!(mcp["description"].as_str().unwrap().starts_with("Manage MCP servers"));
    assert_eq!(
        (mcp["sourceInfo"]["path"].clone(), mcp["sourceInfo"]["source"].clone()),
        (json!("builtin:mcp"), json!("builtin"))
    );

    let handled = rpc.data(json!({"id": "p", "type": "prompt", "message": "/mcp"})).await;
    assert_eq!(handled, json!({"disposition": "handled"}));
    let notice = rpc.wait_for(|record| record["method"] == "notify").await;
    assert!(notice["message"].as_str().unwrap().contains("docs"), "{notice}");
    assert_eq!(notice["notifyType"], "info");

    rpc.data(json!({"id": "l", "type": "prompt", "message": "/mcp logout docs"})).await;
    let logout = rpc.wait_for(|record| record["method"] == "notify" && record["message"] != notice["message"]).await;
    assert_eq!(logout["message"], "No stored credentials for MCP server \"docs\".");
    assert!(rpc.records_of("agent_start").is_empty(), "/mcp starts no run");
    let usage = rpc.data(json!({"id": "u", "type": "prompt", "message": "/mcp a b c"})).await;
    assert_eq!(usage["disposition"], "handled");
    rpc.wait_for(|record: &Value| record["method"] == "notify" && record["notifyType"] == "warning").await;
    rpc.finish().await;
}

#[tokio::test]
async fn lists_no_mcp_command_when_mcp_is_off() {
    let mut rpc = start_with(Options::default(), Default::default(), |_, app| {
        app.resources.disabled_builtin_extensions = vec!["mcp".into()];
    })
    .await;
    let commands = rpc.data(json!({"id": "c", "type": "get_commands"})).await["commands"].clone();
    assert!(commands.as_array().unwrap().iter().all(|command| command["name"] != "mcp"), "{commands}");
    rpc.finish().await;
}
