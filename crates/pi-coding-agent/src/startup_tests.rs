use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::platform::test_http::{TestReply, TestServer, client};
use crate::update::{GITHUB_REPO, ReleaseSource};

fn options(server: &TestServer, dir: &Path, cargo_home: &Path) -> SelfUpdateOptions {
    let executable = dir.join("pi");
    std::fs::write(&executable, b"binary").unwrap();
    SelfUpdateOptions {
        source: ReleaseSource { api_url: server.url.clone(), repo: GITHUB_REPO.into(), client: client(), token: None },
        current_version: "1.0.0".into(),
        force: false,
        executable,
        target: Some("x86_64-unknown-linux-musl".into()),
        cargo_home: Some(cargo_home.to_path_buf()),
        home: dir.to_path_buf(),
        cancel: CancellationToken::new(),
    }
}

async fn run(options: &SelfUpdateOptions) -> (i32, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = self_update_command(options, &mut out, &mut err).await;
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

#[tokio::test]
async fn self_update_reports_up_to_date_installs_and_refuses_cargo_installs() {
    let server = TestServer::start(|request| match request.path() {
        "/repos/yannelli/pi-rust/releases/latest" => {
            TestReply::json(&json!({"tag_name": "v1.0.0", "prerelease": false, "assets": []}))
        }
        _ => TestReply::status(404, "{}"),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let release = options(&server, dir.path(), &dir.path().join("cargo"));
    assert_eq!(run(&release).await, (0, "pi is already up to date (v1.0.0)\n".into(), String::new()));

    let cargo_home = dir.path().join("cargo");
    std::fs::create_dir_all(cargo_home.join("bin")).unwrap();
    let cargo = options(&server, &cargo_home.join("bin"), &cargo_home);
    let (code, out, err) = run(&cargo).await;
    assert_eq!((code, out.as_str()), (1, ""));
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines[0], "error: pi cannot self-update this installation.");
    assert_eq!(
        lines[1],
        format!("pi was installed with cargo. Update it with: {}", crate::update::CARGO_UPDATE_COMMAND)
    );
    assert!(lines[2].starts_with("Installed at: "));
    assert_eq!(server.requests().len(), 1, "cargo installs never ask GitHub");
}

#[tokio::test]
async fn self_update_reports_unreachable_release_servers() {
    let server = TestServer::start(|_| TestReply::status(500, "{}")).await;
    let dir = tempfile::tempdir().unwrap();
    let (code, _, err) = run(&options(&server, dir.path(), &dir.path().join("cargo"))).await;
    assert_eq!(code, 1);
    assert!(err.starts_with("Error: Could not determine latest pi version"), "{err}");
}

#[test]
fn the_startup_changelog_is_computed_once() {
    let settings = SettingsManager::in_memory(json!({}));
    let first = startup_changelog_once(&settings, false);
    assert!(first.record_version && first.report_install);
    assert_eq!(settings.last_changelog_version().as_deref(), Some(VERSION));
    let other = SettingsManager::in_memory(json!({"lastChangelogVersion": "0.0.1"}));
    assert_eq!(startup_changelog_once(&other, true), first);
    assert_eq!(other.last_changelog_version().as_deref(), Some("0.0.1"));
}

#[test]
fn reload_saves_implicit_trust_once_for_the_startup_cwd() {
    let root = tempfile::tempdir().unwrap();
    let (home, cwd, agent) = (root.path().join("home"), root.path().join("work"), root.path().join("agent"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    assert_eq!(auto_trust_on_reload_cwd(&cwd, Some(true), &home), None);
    let mut auto = auto_trust_on_reload_cwd(&cwd, None, &home);
    assert_eq!(auto.as_deref(), Some(cwd.as_path()));

    // Nothing needs trust yet, or the project is not trusted: nothing is saved.
    assert_eq!(save_implicit_project_trust_after_reload(&mut auto, &cwd, true, &agent, &home), Ok(false));
    std::fs::create_dir_all(cwd.join(".pi")).unwrap();
    std::fs::write(cwd.join(".pi/settings.json"), "{}").unwrap();
    assert_eq!(auto_trust_on_reload_cwd(&cwd, None, &home), None);
    assert_eq!(save_implicit_project_trust_after_reload(&mut auto, &cwd, false, &agent, &home), Ok(false));
    let other = root.path().join("other");
    assert_eq!(save_implicit_project_trust_after_reload(&mut auto, &other, true, &agent, &home), Ok(false));

    assert_eq!(save_implicit_project_trust_after_reload(&mut auto, &cwd, true, &agent, &home), Ok(true));
    assert_eq!(ProjectTrustStore::new(&agent).get(&cwd).unwrap(), Some(true));
    assert_eq!(auto, None);
    assert_eq!(save_implicit_project_trust_after_reload(&mut auto, &cwd, true, &agent, &home), Ok(false));
    assert!(reload_status(true).ends_with("; saved project trust"));
    assert!(!reload_status(false).contains("trust"));

    // A saved decision is kept as is.
    let mut auto = Some(cwd.clone());
    ProjectTrustStore::new(&agent).set(&cwd, Some(false)).unwrap();
    assert_eq!(save_implicit_project_trust_after_reload(&mut auto, &cwd, true, &agent, &home), Ok(false));
    assert_eq!(auto, None);
}
