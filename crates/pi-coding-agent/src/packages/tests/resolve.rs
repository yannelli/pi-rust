use serde_json::json;

use super::*;
use crate::packages::{MissingSourceAction, MissingSourceHandler, collect_extension_package_warnings};
use crate::resources::{ResolvedResource, ResourceLoader, ResourceLoaderOptions, SourceOrigin, SourceScope};

fn find<'a>(list: &'a [ResolvedResource], suffix: &str) -> Option<&'a ResolvedResource> {
    list.iter().find(|resource| posix(&resource.path).ends_with(suffix))
}

fn enabled(list: &[ResolvedResource], suffix: &str) -> bool {
    find(list, suffix).is_some_and(|resource| resource.enabled)
}

fn disabled(list: &[ResolvedResource], suffix: &str) -> bool {
    find(list, suffix).is_some_and(|resource| !resource.enabled)
}

fn conventional_package(dir: &Path) {
    write(&dir.join("extensions/index.ts"), EXTENSION);
    write(&dir.join("skills/review/SKILL.md"), &skill("review"));
    write(&dir.join("prompts/fix.md"), "Fix $1");
    write(&dir.join("themes/night.json"), "{\"name\": \"night\"}");
}

#[tokio::test]
async fn local_directory_packages_use_conventional_directories() {
    let fixture = Fixture::new();
    let pkg = fixture.root().join("pkg");
    conventional_package(&pkg);
    fixture.set_global(json!({"packages": ["../pkg"]}));
    let resolved = fixture.manager(MockRunner::failing()).resolve_package_resources(None).await.unwrap();
    assert!(enabled(&resolved.extensions, "pkg/extensions/index.ts"));
    assert!(enabled(&resolved.skills, "pkg/skills/review/SKILL.md"));
    assert!(enabled(&resolved.prompts, "pkg/prompts/fix.md"));
    assert!(enabled(&resolved.themes, "pkg/themes/night.json"));
    let metadata = &resolved.extensions[0].metadata;
    assert_eq!(
        (metadata.source.as_str(), metadata.scope, metadata.origin),
        ("../pkg", SourceScope::User, SourceOrigin::Package)
    );
    assert_eq!(metadata.package_root.as_deref(), Some(pkg.as_path()));
}

#[tokio::test]
async fn package_resources_feed_the_resource_loader() {
    let fixture = Fixture::new();
    let pkg = fixture.root().join("pkg");
    conventional_package(&pkg);
    fixture.set_global(json!({"packages": [pkg.display().to_string()]}));
    let manager = fixture.manager(MockRunner::failing());
    let mut options = ResourceLoaderOptions::new(&fixture.cwd, &fixture.agent_dir);
    options.home = fixture.root();
    options.package_resources = manager.resolve_package_resources(None).await.unwrap();
    let mut loader = ResourceLoader::new(options);
    loader.reload();
    let loaded = loader.resources();
    assert!(loaded.skills.iter().any(|skill| skill.name == "review"));
    assert!(loaded.prompts.iter().any(|prompt| prompt.name == "fix"));
    assert!(loaded.themes.iter().any(|theme| theme.name.as_deref() == Some("night")));
    let extension = loaded.extensions.iter().find(|entry| posix(&entry.path).ends_with("extensions/index.ts")).unwrap();
    assert_eq!(extension.source_info.source, pkg.display().to_string());
}

#[tokio::test]
async fn extension_sources_resolve_builtins_files_and_manifests() {
    let fixture = Fixture::new();
    let pkg = fixture.root().join("manifest-pkg");
    write(&pkg.join("src/main.ts"), EXTENSION);
    write(&pkg.join("extensions/ignored.ts"), EXTENSION);
    write(&pkg.join("package.json"), &json!({"name": "m", "pi": {"extensions": ["./src/main.ts"]}}).to_string());
    let single = fixture.root().join("single.ts");
    write(&single, EXTENSION);
    let sources = vec!["builtin:mcp".to_string(), pkg.display().to_string(), single.display().to_string()];
    let resolved =
        fixture.manager(MockRunner::failing()).resolve_extension_sources(&sources, false, true).await.unwrap();
    let paths: Vec<String> = resolved.extensions.iter().map(|resource| posix(&resource.path)).collect();
    assert!(paths.iter().any(|path| path.ends_with("manifest-pkg/src/main.ts")));
    assert!(paths.iter().any(|path| path.ends_with("single.ts")));
    assert!(!paths.iter().any(|path| path.ends_with("ignored.ts")));
    assert_eq!(paths.last().map(String::as_str), Some("builtin:mcp"));
    assert!(resolved.extensions.iter().all(|resource| resource.metadata.scope == SourceScope::Temporary));
}

#[tokio::test]
async fn manifest_globs_sort_and_skip_hidden_paths() {
    let fixture = Fixture::new();
    let pkg = fixture.root().join("globs");
    for file in ["extension-files/z.ts", "extension-files/a.ts", "extension-files/.ignored.ts"] {
        write(&pkg.join(file), EXTENSION);
    }
    write(&pkg.join("extension-files/nested/.hidden.ts"), EXTENSION);
    write(&pkg.join("extension-groups/group/index.ts"), EXTENSION);
    write(&pkg.join("plugins/local/skills/local-skill/SKILL.md"), &skill("local-skill"));
    write(&pkg.join("plugins/other/skills/other-skill/SKILL.md"), &skill("other-skill"));
    let manifest = json!({"name": "g", "pi": {
        "extensions": ["./extension-files/*.ts", "./extension-files/**/.ignored.ts", "./extension-files/nested/.hidden.ts", "./extension-groups/*/"],
        "skills": ["./plugins/*/skills", "!**/other-skill"]
    }});
    write(&pkg.join("package.json"), &manifest.to_string());
    let resolved = fixture
        .manager(MockRunner::failing())
        .resolve_extension_sources(&[pkg.display().to_string()], false, false)
        .await
        .unwrap();
    let relative: Vec<String> =
        resolved.extensions.iter().map(|resource| posix(resource.path.strip_prefix(&pkg).unwrap())).collect();
    assert_eq!(
        relative,
        [
            "extension-files/a.ts",
            "extension-files/z.ts",
            "extension-files/nested/.hidden.ts",
            "extension-groups/group/index.ts"
        ]
    );
    assert!(enabled(&resolved.skills, "local-skill/SKILL.md"));
    assert!(find(&resolved.skills, "other-skill/SKILL.md").is_none());
}

#[tokio::test]
async fn package_filters_layer_over_the_manifest() {
    let fixture = Fixture::new();
    let pkg = fixture.root().join("layered");
    for name in ["foo", "bar", "baz", "one"] {
        write(&pkg.join(format!("extensions/{name}.ts")), EXTENSION);
    }
    write(&pkg.join("themes/nice.json"), "{}");
    write(&pkg.join("themes/ugly.json"), "{}");
    write(&pkg.join("prompts/a.md"), "a");
    write(
        &pkg.join("package.json"),
        &json!({"name": "l", "pi": {"extensions": ["extensions", "!**/baz.ts"]}}).to_string(),
    );
    let source = pkg.display().to_string();
    fixture.set_global(json!({"packages": [{
        "source": source,
        "extensions": ["!**/bar.ts", "+extensions/one.ts", "-extensions/foo.ts"],
        "skills": [],
        "themes": ["!ugly.json"]
    }]}));
    let resolved = fixture.manager(MockRunner::failing()).resolve_package_resources(None).await.unwrap();
    assert!(disabled(&resolved.extensions, "bar.ts") && disabled(&resolved.extensions, "foo.ts"));
    assert!(enabled(&resolved.extensions, "one.ts"));
    assert!(find(&resolved.extensions, "baz.ts").is_none());
    assert!(enabled(&resolved.themes, "nice.json") && disabled(&resolved.themes, "ugly.json"));
    assert!(enabled(&resolved.prompts, "prompts/a.md"));

    fixture.set_global(json!({"packages": [{"source": source, "extensions": ["extensions/one.ts"], "prompts": []}]}));
    let resolved = fixture.manager(MockRunner::failing()).resolve_package_resources(None).await.unwrap();
    assert!(enabled(&resolved.extensions, "one.ts") && disabled(&resolved.extensions, "bar.ts"));
    assert!(disabled(&resolved.prompts, "prompts/a.md"));
}

#[tokio::test]
async fn autoload_false_project_entries_are_deltas() {
    let fixture = Fixture::new();
    let pkg = fake_npm_package(
        &fixture.agent_dir.join("npm"),
        "pi-tools",
        "1.0.0",
        &[("extensions/foo.ts", EXTENSION), ("extensions/bar.ts", EXTENSION)],
    );
    fixture.set_global(json!({"packages": ["npm:pi-tools"]}));
    fixture
        .settings
        .set_project_packages(&sources(
            json!([{"source": "npm:pi-tools", "autoload": false, "extensions": ["-extensions/foo.ts"]}]),
        ))
        .unwrap();
    let runner = MockRunner::failing();
    let resolved = fixture.manager(runner.clone()).resolve_package_resources(None).await.unwrap();
    assert!(runner.argvs(CallKind::Run).is_empty());
    let foo = find(&resolved.extensions, "extensions/foo.ts").unwrap();
    let bar = find(&resolved.extensions, "extensions/bar.ts").unwrap();
    assert_eq!((foo.enabled, foo.metadata.scope), (false, SourceScope::Project));
    assert_eq!((bar.enabled, bar.metadata.scope), (true, SourceScope::User));
    assert_eq!(foo.path, pkg.join("extensions/foo.ts"));

    let other = fixture.root().join("positive");
    write(&other.join("extensions/foo.ts"), EXTENSION);
    write(&other.join("extensions/bar.ts"), EXTENSION);
    write(&other.join("skills/foo/SKILL.md"), &skill("foo"));
    let packages = json!([{"source": "../../positive", "autoload": false, "extensions": ["+extensions/foo.ts"]}]);
    fixture.set_global(json!({}));
    fixture.settings.set_project_packages(&sources(packages)).unwrap();
    let resolved = fixture.manager(MockRunner::failing()).resolve_package_resources(None).await.unwrap();
    let paths: Vec<_> = resolved.extensions.iter().map(|resource| resource.path.clone()).collect();
    assert_eq!(paths, [other.join("extensions/foo.ts")]);
    assert!(resolved.skills.is_empty());
}

#[tokio::test]
async fn project_entries_replace_user_entries_with_the_same_identity() {
    let fixture = Fixture::new();
    let pkg = fixture.root().join("shared");
    write(&pkg.join("extensions/a.ts"), EXTENSION);
    fixture.set_global(json!({"packages": [pkg.display().to_string(), "../other"]}));
    write(&fixture.agent_dir.join("../other/extensions/b.ts"), EXTENSION);
    fixture.settings.set_project_packages(&sources(json!([pkg.display().to_string()]))).unwrap();
    let resolved = fixture.manager(MockRunner::failing()).resolve_package_resources(None).await.unwrap();
    assert_eq!(find(&resolved.extensions, "shared/extensions/a.ts").unwrap().metadata.scope, SourceScope::Project);
    assert!(enabled(&resolved.extensions, "other/extensions/b.ts"));
    let manager = fixture.manager(MockRunner::failing());
    let deduped = manager
        .dedupe_packages(vec![
            (crate::settings::schema::PackageSource::Source("git:git@github.com:u/r".into()), SourceScope::Project),
            (crate::settings::schema::PackageSource::Source("https://github.com/u/r@v1".into()), SourceScope::User),
            (crate::settings::schema::PackageSource::Source("https://github.com/u/other".into()), SourceScope::User),
        ])
        .unwrap();
    assert_eq!(deduped.len(), 2);
}

#[tokio::test]
async fn missing_npm_packages_install_unless_offline_or_skipped() {
    let fixture = Fixture::new();
    fixture.set_global(json!({"packages": ["npm:remote-tools@^1.0.0"]}));
    let runner = MockRunner::new(|call| {
        let argv = call.argv();
        if argv[1] == "install" {
            let root = install_root(&argv).unwrap();
            fake_npm_package(&root, "remote-tools", "1.2.0", &[("skills/tool/SKILL.md", &skill("tool"))]);
            return Ok(String::new());
        }
        anyhow::bail!("npm unavailable")
    });
    let offline = fixture.offline_manager(runner.clone()).resolve_package_resources(None).await.unwrap();
    assert!(offline.skills.is_empty() && runner.argvs(CallKind::Run).is_empty());

    let skip: MissingSourceHandler = Arc::new(|_| Box::pin(async { MissingSourceAction::Skip }));
    let skipped = fixture.manager(runner.clone()).resolve_package_resources(Some(&skip)).await.unwrap();
    assert!(skipped.skills.is_empty());
    let error: MissingSourceHandler = Arc::new(|_| Box::pin(async { MissingSourceAction::Error }));
    let failed = fixture.manager(runner.clone()).resolve_package_resources(Some(&error)).await;
    assert_eq!(failed.unwrap_err().to_string(), "Missing source: npm:remote-tools@^1.0.0");

    let resolved = fixture.manager(runner.clone()).resolve_package_resources(None).await.unwrap();
    assert!(enabled(&resolved.skills, "remote-tools/skills/tool/SKILL.md"));
    let installs = runner.argvs(CallKind::Run);
    assert_eq!(installs.last().unwrap()[..2], ["npm".to_string(), "install".to_string()]);
    assert!(installs.last().unwrap().contains(&"remote-tools@^1.0.0".to_string()));
    let before = runner.calls().len();
    fixture.manager(runner.clone()).resolve_package_resources(None).await.unwrap();
    assert_eq!(runner.calls().len(), before, "installed packages resolve without npm");
}

#[tokio::test]
async fn a_package_that_fails_to_install_is_skipped_and_reported() {
    let fixture = Fixture::new();
    let pkg = fixture.root().join("pkg");
    conventional_package(&pkg);
    fixture.set_global(json!({"packages": [pkg.display().to_string()]}));
    fixture.settings.set_project_packages(&sources(json!(["npm:missing-tools"]))).unwrap();
    let runner = MockRunner::new(|_| anyhow::bail!("npm install missing-tools failed with code 1"));
    let manager = fixture.manager(runner.clone());
    let resolved = manager.resolve_package_resources(None).await.unwrap();
    assert!(enabled(&resolved.skills, "pkg/skills/review/SKILL.md"));
    assert_eq!(runner.argvs(CallKind::Run).len(), 1);
    let failures = manager.take_install_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!((failures[0].source.as_str(), failures[0].scope), ("npm:missing-tools", SourceScope::Project));
    let app = crate::resources::brand::APP_NAME;
    assert_eq!(
        failures[0].warning(),
        format!(
            "Extension package \"npm:missing-tools\": failed to install (npm install missing-tools failed with code \
             1). Run \"{app} remove npm:missing-tools --local\" to remove it from settings.json."
        )
    );
    assert!(manager.take_install_failures().is_empty());
}

#[tokio::test]
async fn pinned_npm_packages_reinstall_on_version_mismatch() {
    let fixture = Fixture::new();
    fake_npm_package(&fixture.cwd.join(".pi/npm"), "example", "1.0.0", &[]);
    fixture.settings.set_project_packages(&sources(json!(["npm:example@2.0.0"]))).unwrap();
    let runner = MockRunner::new(|_| Ok(String::new()));
    fixture.manager(runner.clone()).resolve_package_resources(None).await.unwrap();
    let installs = runner.argvs(CallKind::Run);
    assert_eq!(installs.len(), 1);
    assert_eq!(install_root(&installs[0]).unwrap(), fixture.cwd.join(".pi/npm"));
}

#[tokio::test]
async fn full_resolve_adds_settings_entries_and_builtins() {
    let fixture = Fixture::new();
    write(&fixture.agent_dir.join("prompts/user.md"), "u");
    write(&fixture.cwd.join(".pi/prompts/project.md"), "p");
    fixture.set_global(json!({"extensions": ["-builtin:llama.cpp"], "prompts": ["-prompts/user.md"]}));
    let mut options = PackageManagerOptions::new(&fixture.cwd, &fixture.agent_dir, fixture.settings.clone());
    options.home = fixture.root();
    options.runner = MockRunner::failing();
    options.builtin_extensions = vec!["mcp".into(), "llama.cpp".into()];
    let resolved = PackageManager::new(options).resolve(None).await.unwrap();
    assert!(enabled(&resolved.extensions, "builtin:mcp") && disabled(&resolved.extensions, "builtin:llama.cpp"));
    assert!(disabled(&resolved.prompts, "prompts/user.md") && enabled(&resolved.prompts, "prompts/project.md"));
}

#[test]
fn host_provided_dependencies_produce_warnings() {
    let fixture = Fixture::new();
    let pkg = fixture.root().join("bad");
    let manifest =
        json!({"name": "bad", "dependencies": {"typebox": "*", "@earendil-works/pi-ai": "1", "left-pad": "1"}});
    write(&pkg.join("package.json"), &manifest.to_string());
    let mut metadata = crate::resources::PathMetadata::new("bad", SourceScope::User, SourceOrigin::Package);
    metadata.package_root = Some(pkg.clone());
    let resource = ResolvedResource { path: pkg.join("index.ts"), enabled: true, metadata };
    let warnings = collect_extension_package_warnings(&[resource.clone(), resource]);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].1.contains("not dependencies: @earendil-works/pi-ai, typebox."));
}

// Upstream issue 11375: `-e npm:pkg` reused the first download forever.
#[tokio::test]
async fn temporary_npm_sources_refresh_when_a_newer_version_exists() {
    let fixture = Fixture::new();
    let runner =
        MockRunner::new(|call| Ok(if call.kind == CallKind::Capture { "\"2.0.0\"".into() } else { String::new() }));
    let manager = fixture.manager(runner.clone());
    let root = manager.npm_install_root(SourceScope::Temporary, true).unwrap();
    fake_npm_package(&root, "pkg", "1.0.0", &[("extensions/index.ts", EXTENSION)]);
    let sources = vec!["npm:pkg".to_string()];
    let resolved = manager.resolve_extension_sources(&sources, false, true).await.unwrap();
    assert_eq!(resolved.extensions.len(), 1);
    let root_text = root.display().to_string();
    assert_eq!(runner.argvs(CallKind::Capture), [["npm", "view", "pkg", "version", "--json"]]);
    assert_eq!(
        runner.argvs(CallKind::Run),
        [["npm", "install", "pkg@latest", "--prefix", &root_text, "--legacy-peer-deps"]]
    );

    // Pinned versions and offline runs keep the cached install without asking npm.
    let pinned = MockRunner::new(|_| anyhow::bail!("npm must not run"));
    fake_npm_package(&root, "pinned", "1.0.0", &[("extensions/index.ts", EXTENSION)]);
    let manager = fixture.manager(pinned.clone());
    manager.resolve_extension_sources(&["npm:pinned@1.0.0".to_string()], false, true).await.unwrap();
    let offline = MockRunner::new(|_| anyhow::bail!("npm must not run"));
    fixture.offline_manager(offline.clone()).resolve_extension_sources(&sources, false, true).await.unwrap();
    assert!(pinned.calls().is_empty() && offline.calls().is_empty());
}
