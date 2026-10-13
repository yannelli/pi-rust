//! Git packages against a real local repository. `url.<dir>.insteadOf` (through
//! `GIT_CONFIG_*` variables) maps `https://github.com/test/` to the test's repositories.

use serde_json::json;

use super::*;
use crate::resources::SourceScope;

fn git(args: &[&str], cwd: &Path) -> String {
    let output = std::process::Command::new("git")
        .args(["-c", "user.email=test@test.com", "-c", "user.name=Test", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit(repo: &Path, file: &str, content: &str, message: &str) -> String {
    write(&repo.join(file), content);
    git(&["add", "."], repo);
    git(&["commit", "-q", "-m", message], repo);
    git(&["rev-parse", "HEAD"], repo)
}

struct Remote {
    fixture: Fixture,
    repo: PathBuf,
    runner: Arc<MockRunner>,
}

/// A fixture whose `https://github.com/test/extension` is a local repository.
fn remote() -> Option<Remote> {
    if !git_available() {
        eprintln!("git not found; skipping git package test");
        return None;
    }
    let fixture = Fixture::new();
    let remotes = fixture.root().join("remotes");
    let repo = remotes.join("extension");
    std::fs::create_dir_all(&repo).unwrap();
    git(&["init", "-q", "--initial-branch=main"], &repo);
    commit(&repo, "extensions/index.ts", EXTENSION, "Initial commit");
    let remotes = posix(&remotes);
    let base = if remotes.starts_with('/') { format!("file://{remotes}/") } else { format!("file:///{remotes}/") };
    let env = vec![
        ("GIT_CONFIG_COUNT".to_string(), "2".to_string()),
        ("GIT_CONFIG_KEY_0".to_string(), format!("url.{base}.insteadOf")),
        ("GIT_CONFIG_VALUE_0".to_string(), "https://github.com/test/".to_string()),
        ("GIT_CONFIG_KEY_1".to_string(), "protocol.file.allow".to_string()),
        ("GIT_CONFIG_VALUE_1".to_string(), "always".to_string()),
    ];
    let runner = MockRunner::with_real_git(env, |call| anyhow::bail!("unexpected {:?}", call.argv()));
    Some(Remote { fixture, repo, runner })
}

#[tokio::test]
async fn install_update_and_remove_a_git_package() {
    let Some(Remote { fixture, repo, runner }) = remote() else { return };
    let manager = fixture.manager(runner.clone());
    manager.install_and_persist("git:github.com/test/extension", false).await.unwrap();
    let installed = fixture.agent_dir.join("git/github.com/test/extension");
    assert_eq!(std::fs::read_to_string(installed.join("extensions/index.ts")).unwrap(), EXTENSION);
    assert_eq!(fixture.global_packages(), json!(["git:github.com/test/extension"]));
    let resolved = manager.resolve_package_resources(None).await.unwrap();
    assert_eq!(resolved.extensions[0].path, installed.join("extensions/index.ts"));

    assert!(manager.check_for_available_updates().await.is_empty());
    let second = commit(&repo, "extensions/index.ts", "// v2", "Second commit");
    let updates = manager.check_for_available_updates().await;
    assert_eq!(updates.len(), 1);
    assert_eq!((updates[0].display_name.as_str(), updates[0].scope), ("github.com/test/extension", SourceScope::User));
    write(&installed.join("untracked.txt"), "stale");
    manager.update(None).await.unwrap();
    assert_eq!(git(&["rev-parse", "HEAD"], &installed), second);
    assert!(!installed.join("untracked.txt").exists());

    git(&["reset", "-q", "--hard", "HEAD~1"], &repo);
    let rewritten = commit(&repo, "extensions/index.ts", "// rewrite", "Rewrite");
    manager.update(Some("git:github.com/test/extension")).await.unwrap();
    assert_eq!(git(&["rev-parse", "HEAD"], &installed), rewritten);

    assert!(manager.remove_and_persist("https://github.com/test/extension", false).await.unwrap());
    assert!(!installed.exists() && !fixture.agent_dir.join("git/github.com").exists());
    assert_eq!(fixture.global_packages(), json!([]));
}

#[tokio::test]
async fn pinned_refs_stay_until_the_configured_ref_changes() {
    let Some(Remote { fixture, repo, runner }) = remote() else { return };
    git(&["tag", "v1"], &repo);
    let v1 = git(&["rev-parse", "HEAD"], &repo);
    let v2 = commit(&repo, "extensions/index.ts", "// v2", "Second commit");
    git(&["tag", "v2"], &repo);
    let manager = fixture.manager(runner.clone());
    manager.install_and_persist("git:github.com/test/extension@v1", false).await.unwrap();
    let installed = fixture.agent_dir.join("git/github.com/test/extension");
    assert_eq!(git(&["rev-parse", "HEAD"], &installed), v1);
    commit(&repo, "extensions/index.ts", "// v3", "Third commit");
    manager.update(None).await.unwrap();
    assert_eq!(git(&["rev-parse", "HEAD"], &installed), v1);
    assert!(manager.check_for_available_updates().await.is_empty());

    assert!(manager.add_source_to_settings("git:github.com/test/extension@v2", false).unwrap());
    manager.update(Some("git:github.com/test/extension")).await.unwrap();
    assert_eq!(git(&["rev-parse", "HEAD"], &installed), v2);
}

#[tokio::test]
async fn temporary_git_sources_refresh_unless_offline() {
    let Some(Remote { fixture, repo, runner }) = remote() else { return };
    let source = vec!["git:github.com/test/extension".to_string()];
    let manager = fixture.manager(runner.clone());
    let first = manager.resolve_extension_sources(&source, false, true).await.unwrap();
    let path = first.extensions[0].path.clone();
    assert!(path.starts_with(fixture.agent_dir.join("tmp/extensions")));
    let checkout = path.parent().unwrap().parent().unwrap().to_path_buf();
    let second = commit(&repo, "extensions/index.ts", "// v2", "Second commit");

    let calls = runner.calls().len();
    fixture.offline_manager(runner.clone()).resolve_extension_sources(&source, false, true).await.unwrap();
    assert_eq!(runner.calls().len(), calls, "offline resolves use the cached checkout");
    manager.resolve_extension_sources(&source, false, true).await.unwrap();
    assert_eq!(git(&["rev-parse", "HEAD"], &checkout), second);
}
