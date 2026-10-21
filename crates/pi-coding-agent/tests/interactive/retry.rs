//! `/retry` (pi-rust extension): the newest prompt of the current branch runs again on a new
//! branch from its parent, over the faux provider.

use pi_ai::Message;
use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::session::EntryData;

use crate::harness::{Options, Ui, local, start};

const EXTENSION: &str = r#"export default function (pi) {
	pi.registerCommand("ext-ask", {
		description: "Send a user message",
		handler: async () => { pi.sendUserMessage("from the extension"); },
	});
	pi.registerCommand("ext-note", {
		description: "Send a custom message",
		handler: async () => { pi.sendMessage({ customType: "note", content: "custom note", display: true }); },
	});
}
"#;

/// Submits `prompt` and waits until the run that ends with `reply` settles.
async fn run_prompt(ui: &Ui, prompt: &str, reply: &str) {
    ui.submit(prompt).await;
    ui.wait_for(reply).await;
    ui.wait_until("the run to settle", |ui| ui.session().is_idle()).await;
}

/// The user and assistant texts of model request `index`.
fn request_texts(ui: &Ui, index: usize) -> Vec<String> {
    let requests = ui.provider.requests();
    let texts = requests[index].messages.iter().filter_map(|message| match message {
        Message::User(user) => Some(user.content.text()),
        Message::Assistant(assistant) => Some(assistant.text()),
        _ => None,
    });
    texts.collect()
}

/// `(entry id, parent id)` of every user message entry with `text`, in file order.
fn prompt_entries(ui: &Ui, text: &str) -> Vec<(String, Option<String>)> {
    let session = ui.session();
    let manager = session.session_manager();
    let entries = manager.entries().iter().filter(|entry| match entry.data() {
        EntryData::Message(Message::User(user)) => user.content.text() == text,
        _ => false,
    });
    entries.map(|entry| (entry.id().to_string(), entry.parent_id().map(String::from))).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_runs_the_last_prompt_again_on_a_new_branch() {
    local(async {
        let responses = vec![FauxResponse::text("first answer"), FauxResponse::text("second answer")];
        let ui = start(Options { responses, ..Options::default() }).await;
        run_prompt(&ui, "same question", "first answer").await;
        run_prompt(&ui, "/retry", "second answer").await;

        assert_eq!(ui.provider.requests().len(), 2);
        assert_eq!(request_texts(&ui, 1), ["same question"], "the old answer is not in the new request");
        let screen = ui.screen();
        assert!(screen.contains("same question") && !screen.contains("first answer"), "{screen}");

        let prompts = prompt_entries(&ui, "same question");
        assert_eq!(prompts.len(), 2, "both branches stay in the tree");
        assert_eq!(prompts[0].1, prompts[1].1, "the new prompt is a sibling of the old one");
        let session = ui.session();
        let manager = session.session_manager();
        let branch: Vec<&str> = manager.get_branch(None).iter().map(|entry| entry.id()).collect();
        assert!(branch.contains(&prompts[1].0.as_str()) && !branch.contains(&prompts[0].0.as_str()));
        let old_answer = manager.entries().iter().any(|entry| match entry.data() {
            EntryData::Message(Message::Assistant(assistant)) => assistant.text() == "first answer",
            _ => false,
        });
        assert!(old_answer, "the old answer is kept for /tree");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_runs_a_prompt_again_after_a_failed_response() {
    local(async {
        let responses = vec![FauxResponse::Error("400 invalid request".into()), FauxResponse::text("works now")];
        let ui = start(Options { responses, ..Options::default() }).await;
        ui.submit("flaky question").await;
        ui.wait_until("the failed run to settle", |ui| ui.provider.requests().len() == 1 && ui.session().is_idle())
            .await;
        run_prompt(&ui, "/retry", "works now").await;

        assert_eq!(ui.provider.requests().len(), 2);
        assert_eq!(request_texts(&ui, 1), ["flaky question"], "the failed answer is not in the new request");
        assert_eq!(prompt_entries(&ui, "flaky question").len(), 2, "both branches stay in the tree");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_needs_a_prompt_and_an_idle_agent() {
    local(async {
        let ui = start(Options { responses: vec![FauxResponse::Hang], ..Options::default() }).await;
        ui.submit("/retry").await;
        ui.wait_for("Error: Nothing to retry").await;
        assert!(ui.provider.requests().is_empty());

        ui.submit("slow question").await;
        ui.wait_until("streaming", |ui| ui.session().is_streaming()).await;
        ui.submit("/retry").await;
        ui.wait_for("Error: Wait for the current response to finish").await;
        assert_eq!(ui.provider.requests().len(), 1);
        assert_eq!(prompt_entries(&ui, "slow question").len(), 1);
        ui.send("\x1b").await;
        ui.wait_until("the abort", |ui| !ui.session().is_streaming()).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_skips_extension_prompts_and_custom_messages() {
    local(async {
        let responses = vec![
            FauxResponse::text("answer one"),
            FauxResponse::text("answer two"),
            FauxResponse::text("answer three"),
        ];
        let files = vec![(".pi/extensions/retry.ts", EXTENSION)];
        let ui = start(Options { responses, files, ..Options::default() }).await;
        run_prompt(&ui, "real question", "answer one").await;
        run_prompt(&ui, "/ext-ask", "answer two").await;
        ui.submit("/ext-note").await;
        ui.wait_for("custom note").await;

        run_prompt(&ui, "/retry", "answer three").await;
        assert_eq!(ui.provider.requests().len(), 3);
        assert_eq!(request_texts(&ui, 2), ["real question"]);
        assert_eq!(prompt_entries(&ui, "real question").len(), 2);
        assert_eq!(prompt_entries(&ui, "from the extension").len(), 1);
    })
    .await;
}
