use std::time::Duration;

use serde_json::json;

use super::*;
use crate::packages::PackageKind;
use crate::resources::SourceScope;

fn view_runner(response: &'static str) -> Arc<MockRunner> {
    MockRunner::new(move |call| match call.kind {
        CallKind::Capture if call.spec.args.first().map(String::as_str) == Some("view") => Ok(response.to_string()),
        CallKind::Capture => anyhow::bail!("not available"),
        CallKind::Run => Ok(String::new()),
    })
}

#[tokio::test]
async fn npm_ranges_update_with_the_configured_spec() {
    let fixture = Fixture::new();
    let project_npm = fixture.cwd.join(".pi").join("npm");
    fake_npm_package(&project_npm, "example", "1.0.0", &[]);
    fixture.settings.set_project_packages(&sources(json!(["npm:example@^1.0.0"]))).unwrap();
    let runner = view_runner("[\"1.0.0\",\"1.2.0\",\"2.0.0\"]");
    fixture.manager(runner.clone()).update(Some("npm:example")).await.unwrap();
    let capture = &runner.calls()[0];
    assert_eq!(capture.argv(), ["npm", "view", "example@^1.0.0", "version", "--json"]);
    assert_eq!((capture.spec.cwd.as_deref(), capture.spec.timeout.is_some()), (Some(fixture.cwd.as_path()), true));
    let root = project_npm.display().to_string();
    assert_eq!(
        runner.argvs(CallKind::Run),
        [["npm", "install", "example@^1.0.0", "--prefix", &root, "--legacy-peer-deps"]]
    );

    for (installed, response) in [("1.3.1", "[\"1.0.0\",\"1.3.1\",\"1.0.2\"]"), ("2.0.0", "\"1.9.0\"")] {
        fake_npm_package(&project_npm, "example", installed, &[]);
        let runner = view_runner(response);
        fixture.manager(runner.clone()).update(Some("npm:example")).await.unwrap();
        assert!(runner.argvs(CallKind::Run).is_empty(), "{installed} is current");
    }
}

#[tokio::test]
async fn npm_updates_batch_per_scope_and_git_updates_run_in_parallel() {
    let fixture = Fixture::new();
    let user_npm = fixture.agent_dir.join("npm");
    let project_npm = fixture.cwd.join(".pi").join("npm");
    for name in ["user-old", "user-current", "user-unknown"] {
        fake_npm_package(&user_npm, name, "1.0.0", &[]);
    }
    for name in ["project-old", "project-current"] {
        fake_npm_package(&project_npm, name, "1.0.0", &[]);
    }
    let user = json!([
        "npm:user-old",
        "npm:user-current",
        "npm:user-unknown",
        "npm:user-pinned@1.0.0",
        "git:github.com/example/user-repo-a",
        "git:github.com/example/user-repo-b",
        "git:github.com/example/user-repo-pinned@v1"
    ]);
    fixture.set_global(json!({"packages": user}));
    let project = json!([
        "npm:project-old",
        "npm:project-current",
        "npm:project-missing",
        "git:github.com/example/project-repo-a"
    ]);
    fixture.settings.set_project_packages(&sources(project)).unwrap();
    let runner = MockRunner::delayed(Duration::from_millis(20), move |call| {
        let args = &call.spec.args;
        if call.kind == CallKind::Capture {
            return match args[1].as_str() {
                "user-old" | "project-old" => Ok("\"2.0.0\"".into()),
                "user-current" | "project-current" => Ok("\"1.0.0\"".into()),
                _ => anyhow::bail!("registry unavailable"),
            };
        }
        if args[0] == "clone" {
            std::fs::create_dir_all(&args[2]).unwrap();
        }
        Ok(String::new())
    });
    fixture.manager(runner.clone()).update(None).await.unwrap();
    assert_eq!(runner.argvs(CallKind::Capture).len(), 5);
    let npm_runs: Vec<Vec<String>> = runner.argvs(CallKind::Run).into_iter().filter(|argv| argv[0] == "npm").collect();
    let user_root = user_npm.display().to_string();
    let project_root = project_npm.display().to_string();
    assert_eq!(
        npm_runs[0],
        ["npm", "install", "user-old@latest", "user-unknown@latest", "--prefix", &user_root, "--legacy-peer-deps"]
    );
    assert_eq!(
        npm_runs[1],
        [
            "npm",
            "install",
            "project-old@latest",
            "project-missing@latest",
            "--prefix",
            &project_root,
            "--legacy-peer-deps"
        ]
    );
    let clones = runner.argvs(CallKind::Run).into_iter().filter(|argv| argv[1] == "clone").count();
    assert_eq!(clones, 4);
    assert!(runner.peak("npm") > 1, "user and project batches run together");
    assert!(runner.peak("git") > 1, "git updates run in parallel");
}

#[tokio::test]
async fn update_lookups_suggest_source_prefixes() {
    let fixture = Fixture::new();
    let packages = json!(["npm:example", "git:github.com/example/repo"]);
    fixture.settings.set_project_packages(&sources(packages)).unwrap();
    let manager = fixture.manager(MockRunner::failing());
    let npm = manager.update(Some("example")).await.unwrap_err().to_string();
    assert_eq!(npm, "No matching package found for example. Did you mean npm:example?");
    let git = manager.update(Some("github.com/example/repo")).await.unwrap_err().to_string();
    assert_eq!(git, "No matching package found for github.com/example/repo. Did you mean git:github.com/example/repo?");
    let none = manager.update(Some("npm:other")).await.unwrap_err().to_string();
    assert_eq!(none, "No matching package found for npm:other");
}

#[tokio::test]
async fn offline_mode_skips_updates_and_checks() {
    let fixture = Fixture::new();
    fake_npm_package(&fixture.cwd.join(".pi").join("npm"), "example", "1.0.0", &[]);
    fixture.settings.set_project_packages(&sources(json!(["npm:example"]))).unwrap();
    let runner = view_runner("\"9.0.0\"");
    let manager = fixture.offline_manager(runner.clone());
    manager.update(None).await.unwrap();
    assert!(manager.check_for_available_updates().await.is_empty());
    assert!(runner.calls().is_empty());
}

#[tokio::test]
async fn update_checks_report_unpinned_npm_packages() {
    let fixture = Fixture::new();
    fake_npm_package(&fixture.cwd.join(".pi").join("npm"), "example", "1.0.0", &[]);
    fixture.settings.set_project_packages(&sources(json!(["npm:example"]))).unwrap();
    let updates = fixture.manager(view_runner("\"1.2.3\"")).check_for_available_updates().await;
    assert_eq!(updates.len(), 1);
    assert_eq!((updates[0].source.as_str(), updates[0].display_name.as_str()), ("npm:example", "example"));
    assert_eq!((updates[0].kind, updates[0].scope), (PackageKind::Npm, SourceScope::Project));
    assert!(fixture.manager(view_runner("\"0.9.0\"")).check_for_available_updates().await.is_empty());
    fixture.settings.set_project_packages(&sources(json!(["npm:example@1.0.0"]))).unwrap();
    let runner = view_runner("\"1.2.3\"");
    assert!(fixture.manager(runner.clone()).check_for_available_updates().await.is_empty());
    assert!(runner.calls().is_empty(), "pinned packages are not checked");
}

#[tokio::test]
async fn legacy_global_npm_installs_migrate_into_the_managed_root() {
    let fixture = Fixture::new();
    let legacy_root = fixture.root().join("legacy-global/node_modules");
    let legacy = fake_npm_package(legacy_root.parent().unwrap(), "legacy-pkg", "1.0.0", &[]);
    fixture.set_global(json!({"packages": ["npm:legacy-pkg"]}));
    let managed_root = fixture.agent_dir.join("npm");
    let (legacy_text, managed) = (legacy_root.display().to_string(), managed_root.clone());
    let runner = MockRunner::new(move |call| {
        let argv = call.argv();
        match (call.kind, argv[1].as_str()) {
            (CallKind::Capture, "root") => Ok(legacy_text.clone()),
            (CallKind::Run, "install") => {
                fake_npm_package(&managed, "legacy-pkg", "1.0.0", &[]);
                Ok(String::new())
            }
            _ => anyhow::bail!("unexpected {argv:?}"),
        }
    });
    let manager = fixture.manager(runner.clone());
    assert_eq!(manager.installed_path("npm:legacy-pkg", SourceScope::User).await, Some(legacy));
    manager.update(Some("npm:legacy-pkg")).await.unwrap();
    let root = managed_root.display().to_string();
    assert_eq!(
        runner.argvs(CallKind::Run),
        [["npm", "install", "legacy-pkg@latest", "--prefix", &root, "--legacy-peer-deps"]]
    );
    let view_calls = runner.argvs(CallKind::Capture).into_iter().filter(|argv| argv[1] == "view").count();
    assert_eq!(view_calls, 0);
    assert_eq!(
        manager.installed_path("npm:legacy-pkg", SourceScope::User).await,
        Some(managed_root.join("node_modules/legacy-pkg"))
    );
}
