//! `/diff` (pi-rust extension) against real temporary git repositories. Each test skips when
//! `git` is not on `PATH`.

use std::path::Path;
use std::process::Command;

use crate::harness::{Options, Ui, local, start};

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|output| output.status.success())
}

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

/// A repository in the session cwd with `tracked.txt` committed.
fn init_repo(ui: &Ui) {
    let cwd = ui.cwd();
    git(&cwd, &["init", "-q"]);
    git(&cwd, &["config", "user.name", "Test"]);
    git(&cwd, &["config", "user.email", "test@test.com"]);
    git(&cwd, &["config", "commit.gpgsign", "false"]);
    git(&cwd, &["add", "tracked.txt"]);
    git(&cwd, &["commit", "-q", "-m", "init"]);
}

fn options() -> Options {
    Options { files: vec![("tracked.txt", "original line\nsecond line\n")], ..Options::default() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diff_shows_stat_untracked_files_and_colored_changes() {
    if !git_available() {
        return eprintln!("git not found; skipping /diff test");
    }
    local(async {
        let ui = start(options()).await;
        init_repo(&ui);
        std::fs::write(ui.cwd().join("tracked.txt"), "changed line\nsecond line\n").unwrap();
        std::fs::write(ui.cwd().join("untracked.txt"), "new\n").unwrap();
        ui.submit("/diff").await;
        ui.wait_for("+changed line").await;
        let history = ui.history();
        for expected in ["Uncommitted changes", "1 file changed", "Untracked files (1):", "untracked.txt"] {
            assert!(history.contains(expected), "missing {expected:?}:\n{history}");
        }
        assert!(history.contains("-original line") && history.contains(" second line"), "{history}");
        let writes = ui.writes();
        let added = writes.find("+changed line").unwrap();
        assert!(writes[..added].ends_with('m'), "added lines are colored: {:?}", &writes[added - 20..added]);

        ui.submit("/diff --stat").await;
        ui.wait_for("Uncommitted changes (stat)").await;
        assert_eq!(ui.history().matches("+changed line").count(), 1, "--stat leaves out the diff");

        ui.submit("/diff missing.txt").await;
        ui.wait_for("No uncommitted changes in missing.txt").await;
        assert_eq!(ui.provider.requests().len(), 0, "/diff does not prompt the model");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diff_reports_a_clean_tree() {
    if !git_available() {
        return eprintln!("git not found; skipping /diff test");
    }
    local(async {
        let ui = start(options()).await;
        init_repo(&ui);
        ui.submit("/diff").await;
        ui.wait_for("No uncommitted changes").await;
        assert!(!ui.history().contains("Uncommitted changes"), "{}", ui.history());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diff_before_the_first_commit_compares_with_an_empty_tree() {
    if !git_available() {
        return eprintln!("git not found; skipping /diff test");
    }
    local(async {
        let ui = start(options()).await;
        git(&ui.cwd(), &["init", "-q"]);
        git(&ui.cwd(), &["add", "tracked.txt"]);
        ui.submit("/diff").await;
        ui.wait_for("+original line").await;
        assert!(ui.history().contains("new file mode"), "{}", ui.history());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diff_outside_a_repository_shows_one_line() {
    if !git_available() {
        return eprintln!("git not found; skipping /diff test");
    }
    local(async {
        let ui = start(options()).await;
        let inside = Command::new("git").args(["rev-parse", "--git-dir"]).current_dir(ui.cwd()).output().unwrap();
        if inside.status.success() {
            return eprintln!("temporary directory is inside a git repository; skipping");
        }
        ui.submit("/diff").await;
        ui.wait_for("Not a git repository").await;
        assert!(!ui.history().contains("Error:"), "{}", ui.history());
    })
    .await;
}
