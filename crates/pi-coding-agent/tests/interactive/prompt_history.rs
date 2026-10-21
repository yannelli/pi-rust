//! Prompt history across runs (pi-rust extension): a prompt from one run comes back with Up
//! and the reverse search in the next run on the same agent directory.

use pi_ai::providers::faux::FauxResponse;

use crate::harness::{Options, local, start};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompts_from_an_earlier_run_come_back_with_up_and_reverse_search() {
    local(async {
        let shared = tempfile::tempdir().unwrap();
        let agent_dir = shared.path().to_path_buf();
        let responses = vec![FauxResponse::text("first answer"), FauxResponse::text("second answer")];
        let first = start(Options { responses, agent_dir: Some(agent_dir.clone()), ..Options::default() }).await;
        first.submit("remember the alpha prompt").await;
        first.wait_for("first answer").await;
        first.wait_until("the first run", |ui| !ui.session().is_streaming()).await;
        first.submit(" keep this one secret").await;
        first.wait_for("second answer").await;
        first.submit("/quit").await;
        first.wait_until("exit", |ui| ui.mode.is_exited()).await;
        let saved = std::fs::read_to_string(agent_dir.join("prompt-history.jsonl")).unwrap();
        assert_eq!(saved, "\"remember the alpha prompt\"\n", "a leading space keeps a prompt out");

        let second = start(Options { agent_dir: Some(agent_dir.clone()), ..Options::default() }).await;
        second.send("\x1b[A").await;
        second.wait_until("the saved prompt", |ui| ui.screen().contains("remember the alpha prompt")).await;
        second.send("\x1b[B").await;
        second.wait_until("the empty draft", |ui| !ui.screen().contains("remember the alpha prompt")).await;

        second.send("\x12").await;
        second.send("ALPHA").await;
        second
            .wait_until("the search", |ui| ui.screen().contains("(reverse-i-search)'ALPHA': remember the alpha"))
            .await;
        second.send("\r").await;
        second.wait_until("the accepted match", |ui| !ui.screen().contains("reverse-i-search")).await;
        assert!(second.screen().contains("remember the alpha prompt"), "{}", second.screen());
        assert_eq!(second.provider.requests().len(), 0, "Enter accepts the match without submitting it");
    })
    .await;
}
