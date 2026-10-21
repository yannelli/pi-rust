//! `/init` (pi-rust extension): a built-in prompt that asks the agent to write or improve
//! AGENTS.md, sent like typed input.

use pi_ai::Message;
use pi_ai::providers::faux::FauxResponse;

use crate::harness::{Options, Ui, local, start};

/// The text of the user message in the first model request.
fn sent_prompt(ui: &Ui) -> String {
    let requests = ui.provider.requests();
    let user = requests[0].messages.iter().rev().find_map(|message| match message {
        Message::User(user) => Some(user.content.text()),
        _ => None,
    });
    user.expect("a user message")
}

async fn run_init(files: Vec<(&'static str, &'static str)>, input: &str) -> (Ui, String) {
    let ui = start(Options { responses: vec![FauxResponse::text("Init answered")], files, ..Options::default() }).await;
    ui.submit(input).await;
    ui.wait_for("Init answered").await;
    let prompt = sent_prompt(&ui);
    (ui, prompt)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_sends_the_built_in_prompt_before_a_template_of_the_same_name() {
    local(async {
        let template = vec![(".pi/prompts/init.md", "---\ndescription: Mine\n---\nTEMPLATE BODY")];
        let (ui, prompt) = run_init(template, "/init").await;
        let services = ui.session().services().clone();
        assert!(services.resources().resources().prompts.iter().any(|template| template.name == "init"));
        assert!(prompt.contains("write an AGENTS.md file there"), "{prompt}");
        assert!(prompt.contains("Build, test, and lint commands"), "{prompt}");
        assert!(!prompt.contains("TEMPLATE BODY") && !prompt.contains("Additional instructions"), "{prompt}");
        assert!(ui.history().contains("Inspect the project"), "{}", ui.history());
        let saved = ui.session().messages().iter().any(|message| match message {
            Message::User(user) => user.content.text() == prompt,
            _ => false,
        });
        assert!(saved, "the prompt is a session message");
        assert_eq!(ui.provider.requests().len(), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_improves_an_existing_context_file() {
    local(async {
        let (_ui, prompt) = run_init(vec![("AGENTS.md", "# Old notes\n")], "/init").await;
        assert!(prompt.contains("improve its existing AGENTS.md file in place"), "{prompt}");
        assert!(!prompt.contains("write an AGENTS.md file there"), "{prompt}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_appends_extra_instructions() {
    local(async {
        let (_ui, prompt) = run_init(Vec::new(), "/init Mention the release steps.").await;
        assert!(prompt.contains("write an AGENTS.md file there"), "{prompt}");
        assert!(prompt.ends_with("Additional instructions:\nMention the release steps."), "{prompt}");
    })
    .await;
}
