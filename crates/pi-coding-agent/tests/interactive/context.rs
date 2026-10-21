//! `/context` (pi-rust extension): context window use and its estimated breakdown.

use pi_ai::providers::faux::FauxResponse;

use crate::harness::{Options, local, start};

const CATEGORIES: [&str; 8] = [
    "System prompt",
    "Tool definitions",
    "Context files",
    "Skills",
    "User messages",
    "Assistant messages",
    "Tool results",
    "Free space",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_shows_the_window_and_the_breakdown() {
    local(async {
        let agents: &'static str = Box::leak("Keep answers short.\n".repeat(200).into_boxed_str());
        let responses = vec![FauxResponse::text("context answer")];
        let ui = start(Options { responses, files: vec![("AGENTS.md", agents)], ..Options::default() }).await;
        ui.submit("/context").await;
        ui.wait_for("Estimated breakdown").await;
        let history = ui.history();
        let files_row = history.lines().find(|line| line.contains("Context files")).unwrap();
        assert!(files_row.contains("1.0k"), "AGENTS.md is about 1k tokens: {files_row}");
        for expected in ["Context Usage", "local/tiny", "128k tokens", "estimated", "Auto-compact:"] {
            assert!(history.contains(expected), "missing {expected:?}:\n{history}");
        }
        for category in CATEGORIES {
            assert!(history.contains(category), "missing {category:?}:\n{history}");
        }
        assert_eq!(ui.provider.requests().len(), 0, "/context does not prompt the model");

        ui.submit("hello").await;
        ui.wait_for("context answer").await;
        ui.wait_until("idle", |ui| !ui.session().is_streaming()).await;
        ui.submit("/context").await;
        ui.wait_for("from the last response").await;
        assert_eq!(ui.history().matches("Estimated breakdown").count(), 2, "{}", ui.history());
    })
    .await;
}
