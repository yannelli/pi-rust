//! Prompt history across runs (pi-rust extension): a prompt from one run comes back with Up
//! and the reverse search in the next run on the same agent directory.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slash_commands_are_recalled_in_full_instead_of_the_previous_prompt() {
    local(async {
        let ui = start(Options { responses: vec![FauxResponse::text("done")], ..Options::default() }).await;
        ui.submit("the previous ordinary prompt").await;
        ui.wait_for("done").await;
        ui.wait_until("idle", |ui| !ui.session().is_streaming()).await;
        let name = "Long session name 日本語 ".repeat(15).trim().to_string();
        ui.submit(&format!("/name {name}")).await;
        ui.wait_until("named", |ui| ui.session().session_name().as_deref() == Some(name.as_str())).await;
        ui.session().set_session_name("temporary").unwrap();
        ui.send("\x1b[A").await;
        ui.send("\r").await;
        ui.wait_until("recalled full command", |ui| ui.session().session_name().as_deref() == Some(name.as_str()))
            .await;
        assert_eq!(ui.provider.requests().len(), 1);
    })
    .await;
}

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
        assert_eq!(saved, "\"remember the alpha prompt\"\n\"/quit\"\n", "a leading space keeps a prompt out");

        let second = start(Options { agent_dir: Some(agent_dir.clone()), ..Options::default() }).await;
        second.send("\x1b[A").await;
        second.wait_until("the saved command", |ui| ui.screen().contains("/quit")).await;
        second.send("\x1b[A").await;
        second.wait_until("the saved prompt", |ui| ui.screen().contains("remember the alpha prompt")).await;
        second.send("\x1b[B").await;
        second.wait_until("the newer command", |ui| ui.screen().contains("/quit")).await;
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
