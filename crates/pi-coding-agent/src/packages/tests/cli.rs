use serde_json::json;

use super::*;
use crate::packages::{CommandOutput, PackageCommandOptions, SelfUpdateRequest, run_package_command};
use crate::resources::ProjectTrustStore;

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn command_options(fixture: &Fixture, runner: Arc<MockRunner>) -> (PackageCommandOptions, [Arc<Mutex<Vec<u8>>>; 2]) {
    let mut options = PackageCommandOptions::new(fixture.cwd.clone(), fixture.agent_dir.clone());
    options.home = fixture.root();
    options.runner = runner;
    options.offline = Some(false);
    let (stdout, out) = CommandOutput::capture();
    let (stderr, err) = CommandOutput::capture();
    options.stdout = stdout;
    options.stderr = stderr;
    (options, [out, err])
}

async fn run_with(options: &PackageCommandOptions, buffers: &[Arc<Mutex<Vec<u8>>>; 2], line: &str) -> Run {
    let args: Vec<String> = line.split_whitespace().map(String::from).collect();
    let code = run_package_command(&args, options).await.expect("package command");
    let text = |buffer: &Arc<Mutex<Vec<u8>>>| String::from_utf8(std::mem::take(&mut *buffer.lock().unwrap())).unwrap();
    Run { code, stdout: text(&buffers[0]), stderr: text(&buffers[1]) }
}

async fn run(fixture: &Fixture, line: &str) -> Run {
    let (options, buffers) = command_options(fixture, MockRunner::new(|_| Ok(String::new())));
    run_with(&options, &buffers, line).await
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[tokio::test]
async fn install_list_and_remove_local_packages() {
    let fixture = Fixture::new();
    let pkg = fixture.cwd.join("packages/local-package");
    write(&pkg.join("extensions/index.ts"), EXTENSION);
    let installed = run(&fixture, "install ./packages/local-package").await;
    assert_eq!(installed.code, 0, "{}", installed.stderr);
    assert_eq!(installed.stdout, "Installing ./packages/local-package...\nInstalled ./packages/local-package\n");
    let stored = read_json(&fixture.agent_dir.join("settings.json"))["packages"][0].as_str().unwrap().to_string();
    assert_eq!(crate::resources::paths::canonicalize(&fixture.agent_dir.join(stored)), pkg);

    let listed = run(&fixture, "list").await;
    assert_eq!(
        listed.stdout,
        format!(
            "User packages:\n  ../project/packages/local-package\n    {}\n",
            crate::resources::paths::normalize(&pkg).display()
        )
    );
    let removed = run(&fixture, &format!("uninstall {}/", pkg.display())).await;
    assert_eq!(removed.code, 0);
    assert!(removed.stdout.ends_with(&format!("Removed {}/\n", pkg.display())));
    assert_eq!(read_json(&fixture.agent_dir.join("settings.json"))["packages"], json!([]));
    let again = run(&fixture, "remove ./packages/local-package").await;
    assert_eq!((again.code, again.stderr.as_str()), (1, "No matching package found for ./packages/local-package\n"));
    assert_eq!(run(&fixture, "list").await.stdout, "No packages installed.\n");
}

#[tokio::test]
async fn list_resolves_project_trust() {
    let fixture = Fixture::new();
    write(&fixture.cwd.join(".pi/settings.json"), &json!({"packages": ["npm:@project/pkg"]}).to_string());
    let listed = |run: Run| (run.code, run.stdout.contains("Project packages:\n  npm:@project/pkg"));
    assert_eq!(listed(run(&fixture, "list").await), (0, false), "ask without a terminal means untrusted");
    assert_eq!(listed(run(&fixture, "list --approve").await), (0, true));
    ProjectTrustStore::new(&fixture.agent_dir).set(&fixture.cwd, Some(true)).unwrap();
    assert_eq!(listed(run(&fixture, "list").await), (0, true));
    assert_eq!(listed(run(&fixture, "list -na").await), (0, false));
    ProjectTrustStore::new(&fixture.agent_dir).set(&fixture.cwd, None).unwrap();
    write(&fixture.agent_dir.join("settings.json"), &json!({"defaultProjectTrust": "always"}).to_string());
    assert_eq!(listed(run(&fixture, "list").await), (0, true));
    ProjectTrustStore::new(&fixture.agent_dir).set(&fixture.cwd, Some(false)).unwrap();
    assert_eq!(listed(run(&fixture, "list").await), (0, false), "trust.json wins over the default");
}

#[tokio::test]
async fn trust_prompts_and_extension_decisions_apply() {
    let fixture = Fixture::new();
    write(&fixture.cwd.join(".pi/settings.json"), &json!({"packages": ["npm:@project/pkg"]}).to_string());
    let (mut options, buffers) = command_options(&fixture, MockRunner::failing());
    options.has_ui = true;
    options.trust_prompt = Some(Arc::new(|prompt| Box::pin(async move { prompt.options.into_iter().next() })));
    let result = run_with(&options, &buffers, "list").await;
    assert!(result.stdout.contains("Project packages:"));
    assert_eq!(ProjectTrustStore::new(&fixture.agent_dir).get(&fixture.cwd).unwrap(), Some(true));

    let other = Fixture::new();
    write(&other.cwd.join(".pi/settings.json"), &json!({"packages": ["npm:@project/pkg"]}).to_string());
    let (mut options, buffers) = command_options(&other, MockRunner::failing());
    options.trust_extensions = Some(Arc::new(|_| {
        let decision = crate::resources::trust::ExtensionTrustDecision { trusted: true, remember: false };
        Box::pin(async move { (Some(decision), vec!["Failed to load extension \"x\": boom".to_string()]) })
    }));
    let result = run_with(&options, &buffers, "list").await;
    assert!(result.stdout.contains("Project packages:"));
    assert_eq!(result.stderr, "Warning: Failed to load extension \"x\": boom\n");
}

#[tokio::test]
async fn update_uses_only_the_saved_project_trust() {
    let fixture = Fixture::new();
    fixture.set_global(json!({"defaultProjectTrust": "always"}));
    write(&fixture.cwd.join(".pi/settings.json"), &json!({"packages": ["npm:fake-package"]}).to_string());
    let runner = MockRunner::new(|_| Ok(String::new()));
    let (options, buffers) = command_options(&fixture, runner.clone());
    let result = run_with(&options, &buffers, "update --extensions").await;
    assert_eq!((result.code, result.stdout.as_str()), (0, "Updated packages\n"));
    assert!(runner.calls().is_empty());

    ProjectTrustStore::new(&fixture.agent_dir).set(&fixture.cwd, Some(true)).unwrap();
    let result = run_with(&options, &buffers, "update --extensions").await;
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(result.stdout.starts_with("Updating npm:fake-package...\n"));
    let installs = runner.argvs(CallKind::Run);
    assert_eq!(install_root(&installs[0]).unwrap(), fixture.cwd.join(".pi/npm"));
}

#[tokio::test]
async fn local_changes_need_a_trusted_project() {
    let fixture = Fixture::new();
    write(&fixture.cwd.join(".pi/settings.json"), "{}");
    let blocked = run(&fixture, "install -l ./local-package").await;
    assert_eq!(blocked.code, 1);
    assert_eq!(blocked.stderr, "Project is not trusted. Use --approve to modify local package config.\n");

    let fresh = Fixture::new();
    let pkg = fresh.root().join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    let result = run(&fresh, &format!("install -l {}", pkg.display())).await;
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert_eq!(read_json(&fresh.cwd.join(".pi/settings.json"))["packages"], json!(["../../pkg"]));
}

#[tokio::test]
async fn usage_errors_and_help() {
    let fixture = Fixture::new();
    let help = run(&fixture, "install --help").await;
    assert_eq!(help.code, 0);
    assert!(help.stdout.starts_with("Usage:\n  pi install <source> [-l] [--approve|--no-approve]\n"));
    let unknown = run(&fixture, "install --unknown").await;
    assert_eq!(unknown.code, 1);
    assert_eq!(
        unknown.stderr,
        "Unknown option --unknown for \"install\".\nUse \"pi --help\" or \"pi install <source> [-l] [--approve|--no-approve]\".\n"
    );
    let missing = run(&fixture, "install").await;
    assert_eq!(missing.stderr, "Missing install source.\nUsage: pi install <source> [-l] [--approve|--no-approve]\n");
    let conflict = run(&fixture, "update --models --self").await;
    assert!(conflict.stderr.starts_with("--models cannot be combined with --self"));
    for (line, message) in [
        ("update --all npm:x", "--all cannot be combined with a positional source"),
        ("update --extension a --extension b", "--extension can only be provided once"),
        ("update --extension", "Missing value for --extension."),
        ("update a b", "Unexpected argument b."),
        ("update npm:x --self", "positional update targets cannot be combined with --self, --extensions, or --all"),
        ("list -l", "Unknown option -l for \"list\"."),
    ] {
        let result = run(&fixture, line).await;
        assert_eq!(result.code, 1, "{line}");
        assert!(result.stderr.starts_with(message), "{line}: {}", result.stderr);
    }
    let args = vec!["chat".to_string()];
    assert!(
        run_package_command(&args, &PackageCommandOptions::new(fixture.cwd.clone(), fixture.agent_dir.clone()))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn update_targets_call_the_models_and_self_update_owners() {
    let fixture = Fixture::new();
    let (mut options, buffers) = command_options(&fixture, MockRunner::failing());
    let models = run_with(&options, &buffers, "update --models").await;
    assert_eq!(
        (models.code, models.stderr.as_str()),
        (1, "Error: Model catalog refresh is not available in this build.\n")
    );
    let refreshed = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
    let sink = refreshed.clone();
    options.models_update = Some(Arc::new(move |dir| {
        sink.lock().unwrap().push(dir);
        Box::pin(async { Ok(()) })
    }));
    let models = run_with(&options, &buffers, "update --models").await;
    assert_eq!((models.code, models.stdout.as_str()), (0, "Model catalogs refreshed\n"));
    assert_eq!(*refreshed.lock().unwrap(), std::slice::from_ref(&fixture.agent_dir));

    let no_self = run_with(&options, &buffers, "update").await;
    assert_eq!(no_self.stdout, "Extensions are skipped. Run pi update --extensions to update extensions.\n");
    assert_eq!((no_self.code, no_self.stderr.as_str()), (1, "error: pi cannot self-update this installation.\n"));
    let requests = Arc::new(Mutex::new(Vec::<SelfUpdateRequest>::new()));
    let sink = requests.clone();
    options.self_update = Some(Arc::new(move |request| {
        sink.lock().unwrap().push(request);
        Box::pin(async { 0 })
    }));
    fixture.set_global(json!({"npmCommand": ["pnpm"]}));
    let all = run_with(&options, &buffers, "update --all --force").await;
    assert_eq!((all.code, all.stdout.as_str()), (0, "Updated packages\n"));
    let pi = run_with(&options, &buffers, "update pi").await;
    assert_eq!(pi.code, 0);
    let requests = requests.lock().unwrap();
    assert_eq!((requests.len(), requests[0].force, requests[1].force), (2, true, false));
    assert_eq!(requests[0].npm_command, Some(vec!["pnpm".to_string()]));
}

#[tokio::test]
async fn update_errors_suggest_configured_sources() {
    let fixture = Fixture::new();
    fixture.set_global(json!({"packages": ["npm:example"]}));
    let result = run(&fixture, "update example").await;
    assert_eq!(result.code, 1);
    assert_eq!(result.stderr, "Error: No matching package found for example. Did you mean npm:example?\n");
}
