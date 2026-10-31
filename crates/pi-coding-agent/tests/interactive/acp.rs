//! `/acp` on the virtual terminal: install into the project settings and move an entry between
//! scopes. The registry URL refuses connections, so only the built-in agents are listed.

use serde_json::{Value, json};

use crate::harness::{Options, Ui, local, start};

const DOWN: &str = "\x1b[B";
const ENTER: &str = "\r";

fn ids(ui: &Ui, project: bool) -> Vec<String> {
    let settings = ui.session().settings().clone();
    let layer = if project { settings.project_settings() } else { settings.global_settings() };
    let entries = layer.extra.get("acpAgents").and_then(Value::as_array).cloned().unwrap_or_default();
    entries.iter().filter_map(|entry| entry["id"].as_str().map(str::to_string)).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_installs_into_the_chosen_scope_and_moves_entries() {
    local(async {
        let settings = json!({"acpRegistryUrl": "http://127.0.0.1:9/registry.json",
            "acpAgents": [{"id": "custom", "name": "Custom agent", "command": "custom-acp"}]});
        let ui = start(Options { settings, ..Options::default() }).await;
        ui.submit("/acp").await;
        ui.wait_for("Custom agent").await;
        assert!(ui.screen().contains("[global]"), "{}", ui.screen());

        ui.send(ENTER).await;
        ui.wait_for("Move to project settings").await;
        for _ in 0..3 {
            ui.send(DOWN).await;
        }
        ui.send(ENTER).await;
        ui.wait_for("Moved Custom agent to the project settings").await;
        assert_eq!((ids(&ui, false), ids(&ui, true)), (vec![], vec!["custom".to_string()]));

        ui.send("\t").await;
        ui.wait_for("Registry unavailable").await;
        ui.send(DOWN).await;
        ui.send(ENTER).await;
        ui.wait_for("Install Amp in").await;
        ui.send(DOWN).await;
        ui.send(ENTER).await;
        ui.wait_for("Installed Amp in the project settings").await;
        assert_eq!(ids(&ui, true), ["custom", "amp-acp"]);
        assert!(ids(&ui, false).is_empty(), "nothing went to the global settings");
    })
    .await;
}
