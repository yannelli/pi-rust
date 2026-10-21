//! `/review` (pi-rust extension): a built-in prompt that asks the agent to review the
//! uncommitted git changes, sent like typed input. Each test skips when `git` is not on `PATH`.

use std::process::Command;

use pi_ai::Message;
use pi_ai::providers::faux::FauxResponse;

use crate::harness::{Options, Ui, local, start};

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|output| output.status.success())
}

/// Whether the session cwd is inside a git work tree, for example when the temporary folder is.
fn inside_repository(ui: &Ui) -> bool {
    let output = Command::new("git").args(["rev-parse", "--git-dir"]).current_dir(ui.cwd()).output().unwrap();
    output.status.success()
}

/// The text of the last user message in the first model request.
fn sent_prompt(ui: &Ui) -> String {
    let requests = ui.provider.requests();
    let user = requests[0].messages.iter().rev().find_map(|message| match message {
        Message::User(user) => Some(user.content.text()),
        _ => None,
    });
    user.expect("a user message")
}

fn options() -> Options {
    Options { responses: vec![FauxResponse::text("Review answered")], ..Options::default() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_sends_the_git_commands_and_extra_instructions() {
    if !git_available() {
        return eprintln!("git not found; skipping /review test");
    }
    local(async {
        let ui = start(options()).await;
        let init = Command::new("git").args(["init", "-q"]).current_dir(ui.cwd()).output().unwrap();
        assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
        ui.submit("/review Focus on the parser.").await;
        ui.wait_for("Review answered").await;
        let prompt = sent_prompt(&ui);
        assert!(prompt.contains("`git status`") && prompt.contains("`git diff HEAD`"), "{prompt}");
        assert!(prompt.contains("untracked files") && prompt.contains("`path:line`"), "{prompt}");
        assert!(prompt.ends_with("\nFocus on the parser."), "{prompt}");
        assert!(ui.history().contains("Review the uncommitted changes"), "{}", ui.history());
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
async fn review_outside_a_repository_shows_an_error_and_sends_nothing() {
    if !git_available() {
        return eprintln!("git not found; skipping /review test");
    }
    local(async {
        let ui = start(options()).await;
        if inside_repository(&ui) {
            return eprintln!("temporary directory is inside a git repository; skipping");
        }
        ui.submit("/review").await;
        ui.wait_for("Error: Not a git repository").await;
        ui.settle(200).await;
        assert!(ui.provider.requests().is_empty(), "no request without a repository");
        assert!(!ui.history().contains("Review the uncommitted changes"), "{}", ui.history());
    })
    .await;
}
