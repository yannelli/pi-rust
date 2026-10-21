//! The run-finished notification (pi-rust extension): a run that takes longer than
//! `terminal.notifyMinSeconds` writes a notification when it settles, a shorter one does not.

use std::time::Duration;

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::resources::brand::APP_NAME;
use serde_json::json;

use crate::harness::{Options, Ui, local, start};

/// How many notifications the terminal got; `osc` picks the protocol from the environment.
fn notifications(ui: &Ui) -> usize {
    let writes = ui.writes();
    let sequences = [
        format!("\x1b]777;notify;{APP_NAME};Done\x07"),
        format!("\x1b]99;i=1:d=0;{APP_NAME}\x1b\\\x1b]99;i=1:p=body;Done\x1b\\"),
        format!("\x1b]9;{APP_NAME}: Done\x07"),
    ];
    sequences.iter().map(|sequence| writes.matches(sequence.as_str()).count()).sum()
}

async fn run_once(chunk_delay: Duration, min_seconds: f64) -> Ui {
    let settings = json!({"terminal": {"notifyOnFinish": "osc", "notifyMinSeconds": min_seconds}});
    let responses = vec![FauxResponse::text("the slow answer arrives word by word")];
    let ui = start(Options { responses, settings, chunk_delay, ..Options::default() }).await;
    ui.submit("take your time").await;
    ui.wait_for("the slow answer arrives").await;
    ui.wait_until("the run to settle", |ui| !ui.session().is_streaming()).await;
    ui
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_run_notifies_when_it_settles() {
    local(async {
        let ui = run_once(Duration::from_millis(80), 0.25).await;
        ui.wait_until("the notification", |ui| notifications(ui) > 0).await;
        ui.settle(200).await;
        assert_eq!(notifications(&ui), 1, "one notification per run");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_short_run_does_not_notify() {
    local(async {
        let ui = run_once(Duration::ZERO, 5.0).await;
        ui.settle(300).await;
        assert_eq!(notifications(&ui), 0, "{:?}", ui.writes());
    })
    .await;
}
