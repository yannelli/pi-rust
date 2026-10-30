use serde_json::json;

use super::*;
use crate::packages::{ProgressEvent, ProgressKind};
use crate::resources::SourceScope;

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

#[tokio::test]
async fn local_installs_store_paths_relative_to_the_settings_file() {
    let fixture = Fixture::new();
    let pkg = fixture.cwd.join("packages/local-pkg");
    write(&pkg.join("extensions/index.ts"), EXTENSION);
    let manager = fixture.manager(MockRunner::failing());
    manager.install_and_persist("./packages/local-pkg", false).await.unwrap();
    assert_eq!(fixture.global_packages(), json!(["../project/packages/local-pkg"]));
    manager.install_and_persist("./packages/local-pkg", true).await.unwrap();
    assert_eq!(fixture.project_packages(), json!(["../packages/local-pkg"]));
    let missing = manager.install("./does-not-exist", false).await.unwrap_err();
    assert!(missing.to_string().starts_with("Path does not exist: "));

    assert!(manager.remove_and_persist(&format!("{}/", pkg.display()), false).await.unwrap());
    assert_eq!(fixture.global_packages(), json!([]));
    assert!(!manager.remove_and_persist("./packages/local-pkg", false).await.unwrap());
}

#[tokio::test]
async fn settings_entries_replace_refs_and_keep_filters() {
    let fixture = Fixture::new();
    let manager = fixture.manager(MockRunner::failing());
    assert!(manager.add_source_to_settings("git:github.com/user/repo@v1", false).unwrap());
    assert!(!manager.add_source_to_settings("git:github.com/user/repo@v1", false).unwrap());
    assert!(manager.add_source_to_settings("git:github.com/user/repo@v2", false).unwrap());
    assert_eq!(fixture.global_packages(), json!(["git:github.com/user/repo@v2"]));
    let filtered = json!([{"source": "npm:tools@1.0.0", "extensions": ["extensions/main.ts"], "skills": []}]);
    fixture.settings.set_packages(&sources(filtered));
    assert!(manager.add_source_to_settings("npm:tools@2.0.0", false).unwrap());
    assert_eq!(
        fixture.global_packages(),
        json!([{"source": "npm:tools@2.0.0", "extensions": ["extensions/main.ts"], "skills": []}])
    );
    assert!(manager.remove_source_from_settings("npm:tools", false).unwrap());
    assert_eq!(fixture.global_packages(), json!([]));
}

#[tokio::test]
async fn npm_installs_use_the_configured_command_and_managed_root() {
    let fixture = Fixture::new();
    let runner = MockRunner::new(|_| Ok(String::new()));
    fixture.manager(runner.clone()).install("npm:@scope/pkg", false).await.unwrap();
    let root = fixture.agent_dir.join("npm").display().to_string();
    assert_eq!(
        runner.argvs(CallKind::Run)[0],
        strings(&["npm", "install", "@scope/pkg", "--prefix", &root, "--legacy-peer-deps", "--no-audit", "--no-fund"])
    );
    assert!(fixture.agent_dir.join("npm/package.json").exists());
    assert_eq!(std::fs::read_to_string(fixture.agent_dir.join("npm/.gitignore")).unwrap(), "*\n!.gitignore\n");

    fixture.set_global(json!({"npmCommand": ["mise", "exec", "bun@1", "--", "bun"]}));
    let runner = MockRunner::new(|_| Ok(String::new()));
    let manager = fixture.manager(runner.clone());
    manager.install("npm:@scope/pkg", false).await.unwrap();
    manager.remove("npm:@scope/pkg", false).await.unwrap();
    let calls = runner.argvs(CallKind::Run);
    assert_eq!(
        calls[0],
        strings(&["mise", "exec", "bun@1", "--", "bun", "install", "@scope/pkg", "--cwd", &root, "--omit=peer"])
    );
    assert_eq!(calls[1], strings(&["mise", "exec", "bun@1", "--", "bun", "uninstall", "@scope/pkg", "--cwd", &root]));

    fixture.set_global(json!({}));
    let runner = MockRunner::new(|_| Ok(String::new()));
    fixture.manager(runner.clone()).remove("npm:@scope/pkg", false).await.unwrap();
    let expected = strings(&[
        "npm",
        "uninstall",
        "@scope/pkg",
        "--prefix",
        &root,
        "--legacy-peer-deps",
        "--no-audit",
        "--no-fund",
    ]);
    assert_eq!(runner.argvs(CallKind::Run), [expected]);
}

// Upstream #11358: npm writes nested package-lock.json entries for a symlinked --prefix.
#[cfg(unix)]
#[tokio::test]
async fn npm_installs_pass_the_real_path_when_the_agent_dir_is_a_symlink() {
    let fixture = Fixture::new();
    let linked = fixture.root().join("linked-agent");
    std::os::unix::fs::symlink(&fixture.agent_dir, &linked).unwrap();
    let runner = MockRunner::new(|_| Ok(String::new()));
    let mut options = PackageManagerOptions::new(&fixture.cwd, &linked, fixture.settings.clone());
    options.home = fixture.root();
    options.runner = runner.clone();
    options.offline = Some(false);
    let manager = PackageManager::new(options);
    manager.install("npm:@scope/pkg", false).await.unwrap();
    manager.remove("npm:@scope/pkg", false).await.unwrap();
    let root = fixture.agent_dir.join("npm").display().to_string();
    assert_eq!(
        runner.argvs(CallKind::Run),
        [
            strings(&[
                "npm",
                "install",
                "@scope/pkg",
                "--prefix",
                &root,
                "--legacy-peer-deps",
                "--no-audit",
                "--no-fund"
            ]),
            strings(&[
                "npm",
                "uninstall",
                "@scope/pkg",
                "--prefix",
                &root,
                "--legacy-peer-deps",
                "--no-audit",
                "--no-fund"
            ]),
        ]
    );
}

#[tokio::test]
async fn project_installs_require_trust() {
    let fixture = Fixture::with_trust(false);
    let runner = MockRunner::new(|_| Ok(String::new()));
    let error = fixture.manager(runner.clone()).install("npm:pkg", true).await.unwrap_err();
    assert_eq!(error.to_string(), "Project is not trusted; refusing to access project package storage");
    assert!(runner.calls().is_empty());
}

#[tokio::test]
async fn git_installs_clone_and_install_dependencies_without_peers() {
    let fixture = Fixture::new();
    let target = fixture.agent_dir.join("git/github.com/user/repo");
    let clone_target = target.clone();
    let runner = MockRunner::new(move |call| {
        if call.argv()[..2] == ["git".to_string(), "clone".to_string()] {
            write(&clone_target.join("package.json"), "{\"name\": \"repo\", \"version\": \"1.0.0\"}");
        }
        Ok(String::new())
    });
    let events = Arc::new(Mutex::new(Vec::<ProgressEvent>::new()));
    let sink = events.clone();
    let manager = fixture.manager(runner.clone());
    manager.set_progress_callback(Some(Arc::new(move |event| sink.lock().unwrap().push(event.clone()))));
    manager.install("git:github.com/user/repo", false).await.unwrap();
    let calls = runner.calls();
    assert_eq!(
        calls[0].argv(),
        strings(&[
            "git",
            "clone",
            "https://github.com/user/repo",
            &crate::resources::paths::normalize(&target).display().to_string()
        ])
    );
    assert_eq!(
        calls[1].argv(),
        strings(&["npm", "install", "--omit=dev", "--legacy-peer-deps", "--no-audit", "--no-fund"])
    );
    assert_eq!(calls[1].spec.cwd.as_deref(), Some(target.as_path()));
    assert!(fixture.agent_dir.join("git/.gitignore").exists());
    let kinds: Vec<ProgressKind> = events.lock().unwrap().iter().map(|event| event.kind).collect();
    assert_eq!(kinds, [ProgressKind::Start, ProgressKind::Complete]);
    assert_eq!(events.lock().unwrap()[0].message.as_deref(), Some("Installing git:github.com/user/repo..."));
}

#[tokio::test]
async fn failed_git_installs_remove_the_new_checkout() {
    let fixture = Fixture::new();
    let target = fixture.agent_dir.join("git/github.com/user/repo");
    for fail_npm in [false, true] {
        let clone_target = target.clone();
        let runner = MockRunner::new(move |call| {
            let argv = call.argv();
            if argv[1] == "clone" {
                write(&clone_target.join("package.json"), "{}");
                if !fail_npm {
                    anyhow::bail!("simulated git clone failure");
                }
            } else if argv[0] == "npm" {
                anyhow::bail!("simulated dependency install failure");
            }
            Ok(String::new())
        });
        let events = Arc::new(Mutex::new(Vec::<ProgressEvent>::new()));
        let sink = events.clone();
        let manager = fixture.manager(runner);
        manager.set_progress_callback(Some(Arc::new(move |event| sink.lock().unwrap().push(event.clone()))));
        let error = manager.install("git:github.com/user/repo", false).await.unwrap_err();
        assert!(error.to_string().starts_with("simulated"));
        assert!(!target.exists() && !fixture.agent_dir.join("git/github.com").exists());
        assert_eq!(events.lock().unwrap().last().unwrap().kind, ProgressKind::Error);
    }
}

#[tokio::test]
async fn existing_checkouts_reconcile_to_the_configured_ref() {
    let fixture = Fixture::new();
    let target = fixture.agent_dir.join("git/github.com/user/repo");
    write(&target.join("package.json"), "{\"name\": \"repo\"}");
    let runner = MockRunner::new(|call| {
        let args = &call.spec.args;
        match (call.kind, args.first().map(String::as_str), args.get(1).map(String::as_str)) {
            (CallKind::Capture, Some("rev-parse"), Some("HEAD")) => Ok("old-head".into()),
            (CallKind::Capture, Some("rev-parse"), Some("FETCH_HEAD^{commit}")) => Ok("new-head".into()),
            (CallKind::Run, _, _) => Ok(String::new()),
            _ => anyhow::bail!("unexpected {:?}", call.argv()),
        }
    });
    fixture.manager(runner.clone()).install("git:github.com/user/repo@v2", false).await.unwrap();
    let runs = runner.argvs(CallKind::Run);
    assert_eq!(runs[0], strings(&["git", "fetch", "origin", "v2"]));
    assert_eq!(runs[1], strings(&["git", "reset", "--hard", "FETCH_HEAD^{commit}"]));
    assert_eq!(runs[2], strings(&["git", "clean", "-fdx"]));
    assert_eq!(runs[3], strings(&["npm", "install", "--omit=dev", "--legacy-peer-deps", "--no-audit", "--no-fund"]));
    assert!(!target.with_file_name(".repo.pi-update-incomplete").exists());
}

#[tokio::test]
async fn install_paths_stay_inside_their_roots() {
    let fixture = Fixture::new();
    let manager = fixture.manager(MockRunner::failing());
    let source = crate::packages::GitSource {
        repo: "git@evil.example:../../victim/repo".into(),
        host: "evil.example".into(),
        path: "../../victim/repo".into(),
        git_ref: None,
        pinned: false,
    };
    for scope in [SourceScope::User, SourceScope::Project, SourceScope::Temporary] {
        let error = manager.git_install_path(&source, scope).unwrap_err().to_string();
        assert!(error.contains("outside package install root"), "{error}");
    }
    let npm = crate::packages::source::parse_npm_source("left-pad");
    let temp = manager.npm_install_root(SourceScope::Temporary, true).unwrap().join("node_modules").join(&npm.name);
    assert!(temp.starts_with(fixture.agent_dir.join("tmp/extensions/npm")));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(fixture.agent_dir.join("tmp/extensions")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}

#[tokio::test]
async fn removing_a_marketplace_package_from_one_scope_keeps_it_for_the_other() {
    let fixture = Fixture::new();
    let mock = crate::packages::marketplace_test_support::MockMarketplace::start().await;
    mock.publish(&mock.release("v1.0.0", &["todo"], "binary-v1"), true);
    let path = mock.marketplace(&fixture.agent_dir, false).install("todo", None).await.unwrap();
    fixture.set_global(json!({"packages": ["marketplace:todo"]}));
    fixture.settings.set_project_packages(&sources(json!(["marketplace:todo@v1.0.0"]))).unwrap();
    let manager = fixture.manager(MockRunner::failing());
    let binary = manager.marketplace().installed_package("todo").unwrap().unwrap().binary_path;

    assert!(manager.remove_and_persist("marketplace:todo@v1.0.0", true).await.unwrap());
    assert_eq!(fixture.project_packages(), json!([]));
    assert!(path.join("package.json").is_file() && binary.is_file());

    fixture.settings.set_project_packages(&sources(json!(["marketplace:todo"]))).unwrap();
    assert!(manager.remove_and_persist("marketplace:todo", false).await.unwrap());
    assert!(path.join("package.json").is_file() && binary.is_file());

    assert!(manager.remove_and_persist("marketplace:todo", true).await.unwrap());
    assert!(!path.exists() && !binary.exists());
}
