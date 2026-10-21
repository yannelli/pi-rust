//! `--max-turns` (pi-rust run limit) in the interactive mode: a status line, and the session
//! stays usable.

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::agent_session::RunLimits;
use serde_json::json;

use crate::harness::{Options, local, start};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_limit_shows_a_status_line_and_the_next_prompt_runs() {
    local(async {
        let read = FauxResponse::tool_call("r1", "read", json!({"path": "notes.txt"}));
        let responses = vec![read, FauxResponse::text("Second prompt answered")];
        let run_limits = RunLimits { max_turns: Some(1), max_cost: None };
        let files = vec![("notes.txt", "alpha\n")];
        let ui = start(Options { responses, files, run_limits, ..Options::default() }).await;
        ui.submit("read it forever").await;
        ui.wait_for("Stopped after 1 turn (--max-turns)").await;
        ui.wait_until("the run to settle", |ui| !ui.session().is_streaming()).await;
        assert_eq!(ui.provider.requests().len(), 1);

        ui.submit("next").await;
        ui.wait_for("Second prompt answered").await;
        assert_eq!(ui.provider.requests().len(), 2);
    })
    .await;
}
