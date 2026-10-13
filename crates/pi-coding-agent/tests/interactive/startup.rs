//! Renderer modes and the startup trust prompt.

use std::time::Duration;

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::interactive::startup_ui::prompt_project_trust_on;
use pi_coding_agent::resources::trust::ProjectTrustStore;
use pi_coding_agent::settings::schema::TuiMode;
use pi_tui::testing::VirtualTerminal;

use crate::harness::{Options, local, start};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fullscreen_uses_the_alternate_screen_and_regular_does_not() {
    local(async {
        for (tui_mode, alternate) in [(TuiMode::Fullscreen, true), (TuiMode::Regular, false)] {
            let responses = vec![FauxResponse::text("rendered answer")];
            let ui = start(Options { responses, tui_mode, ..Options::default() }).await;
            assert_eq!(ui.mode.tui().is_viewport(), alternate);
            assert_eq!(ui.writes().contains("\x1b[?1049h"), alternate, "{tui_mode:?}");
            ui.submit("show me").await;
            ui.wait_for("rendered answer").await;
            ui.submit("/quit").await;
            ui.wait_until("exit", |ui| ui.mode.is_exited()).await;
        }
    })
    .await;
}

/// A project that needs trust, and the prompt answered with `keys`.
async fn answer_trust_prompt(keys: &'static str) -> (Option<bool>, Option<bool>) {
    let root = tempfile::tempdir().unwrap();
    let (cwd, agent_dir) = (root.path().join("work"), root.path().join("agent"));
    std::fs::create_dir_all(cwd.join(".pi")).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(cwd.join(".pi/settings.json"), "{}").unwrap();
    let terminal = VirtualTerminal::shared(100, 30);
    let input = terminal.clone();
    tokio::task::spawn_local(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        input.borrow().send_input(keys);
    });
    let trusted = prompt_project_trust_on(terminal.clone(), &cwd, &agent_dir, None).await;
    let shown = terminal.borrow().writes().contains("Trust project folder?");
    assert!(shown, "the prompt was not drawn");
    let saved = ProjectTrustStore::new(&agent_dir).get(&cwd).unwrap();
    (trusted, saved)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trust_prompt_saves_the_first_choice() {
    local(async {
        let (trusted, saved) = answer_trust_prompt("\r").await;
        assert_eq!(trusted, Some(true));
        assert_eq!(saved, Some(true));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dismissing_the_trust_prompt_leaves_the_project_untrusted() {
    local(async {
        let (trusted, saved) = answer_trust_prompt("\x1b").await;
        assert_eq!(trusted, Some(false));
        assert_eq!(saved, None);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_trust_prompt_without_project_resources() {
    local(async {
        let root = tempfile::tempdir().unwrap();
        let terminal = VirtualTerminal::shared(80, 24);
        let trusted = prompt_project_trust_on(terminal.clone(), root.path(), root.path(), None).await;
        assert_eq!(trusted, None);
        assert!(terminal.borrow().writes().is_empty());
    })
    .await;
}
