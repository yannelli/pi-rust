use std::time::Duration;

use pi_tui::testing::VirtualTerminal;

use super::*;
use crate::settings::SettingsOptions;
use crate::theme::tests::global_state;

/// Send `keys` one at a time once the dialog had time to draw.
fn feed(terminal: &Rc<RefCell<VirtualTerminal>>, keys: &[&'static str]) {
    let (input, keys) = (terminal.clone(), keys.to_vec());
    tokio::task::spawn_local(async move {
        for key in keys {
            tokio::time::sleep(Duration::from_millis(200)).await;
            input.borrow().send_input(key);
        }
    });
}

async fn local<F: std::future::Future>(future: F) -> F::Output {
    tokio::task::LocalSet::new().run_until(future).await
}

fn settings(dir: &Path) -> SettingsManager {
    SettingsManager::create(dir, dir, SettingsOptions { project_trusted: false })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_time_setup_saves_theme_and_analytics() {
    let _theme = global_state();
    local(async {
        let dir = tempfile::tempdir().unwrap();
        let manager = settings(dir.path());
        let terminal = VirtualTerminal::shared(90, 30);
        feed(&terminal, &["\x1b[B", "\r", "\x1b[B", "\r"]);
        assert!(show_first_time_setup_on(terminal.clone(), &manager).await);
        assert!(terminal.borrow().writes().contains("Pick a theme."));
        assert_eq!(manager.theme_setting().as_deref(), Some("dark"));
        assert!(!manager.enable_analytics());
        let saved = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
        assert!(saved.contains("\"theme\": \"dark\""), "{saved}");

        let skipped = settings(&dir.path().join("other"));
        let terminal = VirtualTerminal::shared(90, 30);
        feed(&terminal, &["\x1b"]);
        assert!(!show_first_time_setup_on(terminal, &skipped).await);
        assert_eq!(skipped.theme_setting(), None);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_cwd_prompt_offers_continue_and_cancel() {
    let _theme = global_state();
    local(async {
        let dir = tempfile::tempdir().unwrap();
        let manager = settings(dir.path());
        for (keys, expected) in [(&["\r"][..], true), (&["\x1b[B", "\r"][..], false), (&["\x1b"][..], false)] {
            let terminal = VirtualTerminal::shared(90, 30);
            feed(&terminal, keys);
            let prompt = "cwd from session file does not exist\n/gone";
            assert_eq!(confirm_missing_session_cwd_on(terminal.clone(), &manager, prompt).await, expected);
            let writes = terminal.borrow().writes();
            assert!(writes.contains("Continue") && writes.contains("Cancel"), "{writes}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_picker_selects_or_cancels() {
    let _theme = global_state();
    local(async {
        let root = tempfile::tempdir().unwrap();
        let (cwd, agent_dir) = (root.path().join("work"), root.path().join("agent"));
        std::fs::create_dir_all(&cwd).unwrap();
        let dir = session_dir(&agent_dir, &cwd);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("2024-01-01_s1.jsonl");
        let header = serde_json::json!({
            "type": "session", "id": "s1", "version": 3, "timestamp": "2024-01-01T00:00:00.000Z",
            "cwd": cwd.display().to_string(),
        });
        std::fs::write(&file, format!("{header}\n")).unwrap();
        let mut session = SessionManager::open_with(&file, None, None).unwrap();
        session.append_session_info("picked session").unwrap();
        session.append_message(&pi_ai::Message::user("hello there")).unwrap();
        let manager = settings(root.path());

        let terminal = VirtualTerminal::shared(100, 30);
        feed(&terminal, &["\r"]);
        let pick = select_session_on(terminal.clone(), &manager, &cwd, None, &agent_dir).await;
        assert_eq!(pick, SessionPick::Selected(file));
        assert!(terminal.borrow().writes().contains("picked session"));

        let terminal = VirtualTerminal::shared(100, 30);
        feed(&terminal, &["\x1b"]);
        assert_eq!(select_session_on(terminal, &manager, &cwd, None, &agent_dir).await, SessionPick::Cancelled);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waits_for_any_key() {
    local(async {
        let terminal = VirtualTerminal::shared(80, 24);
        feed(&terminal, &["x"]);
        tokio::time::timeout(Duration::from_secs(5), wait_for_any_key_on(terminal)).await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trust_prompt_uses_the_startup_settings_theme() {
    let _theme = global_state();
    local(async {
        let root = tempfile::tempdir().unwrap();
        let (cwd, agent_dir) = (root.path().join("work"), root.path().join("agent"));
        std::fs::create_dir_all(cwd.join(".pi")).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(cwd.join(".pi/settings.json"), "{}").unwrap();
        let startup = SettingsManager::create(&cwd, &agent_dir, SettingsOptions { project_trusted: true });
        startup.apply_overrides(serde_json::json!({ "theme": "light" }));
        let terminal = VirtualTerminal::shared(100, 30);
        feed(&terminal, &["\x1b"]);
        let prompt = crate::interactive::startup_ui::prompt_project_trust_with(
            terminal.clone(),
            &cwd,
            &agent_dir,
            None,
            None,
            &startup,
        );
        assert_eq!(prompt.await, Some(false));
        assert!(terminal.borrow().writes().contains("Trust project folder?"));
        assert_eq!(crate::theme::theme().name(), Some("light"));
    })
    .await;
}
