use super::*;
use crate::packages::git_url::parse_git_url;
use crate::packages::manifest::{gallery_info, read_pi_manifest};
use crate::packages::runner::{NpmCommand, git_dependency_install_args, npm_install_args};
use crate::packages::source::{ArchiveFormat, ParsedSource, parse_npm_source, parse_source};
use crate::resources::SourceScope;

fn git(source: &str) -> (String, String, String, Option<String>) {
    let parsed = parse_git_url(source).unwrap_or_else(|| panic!("expected git source: {source}"));
    (parsed.repo, parsed.host, parsed.path, parsed.git_ref)
}

#[test]
fn protocol_urls_parse_without_prefix() {
    let expected = |repo: &str, git_ref: Option<&str>| {
        (
            "x".to_string(),
            "github.com".to_string(),
            "user/repo".to_string(),
            git_ref.map(String::from),
            repo.to_string(),
        )
    };
    for (source, repo, git_ref) in [
        ("https://github.com/user/repo", "https://github.com/user/repo", None),
        ("ssh://git@github.com/user/repo", "ssh://git@github.com/user/repo", None),
        ("https://github.com/user/repo@v1.0.0", "https://github.com/user/repo", Some("v1.0.0")),
        ("git:git@github.com:user/repo", "git@github.com:user/repo", None),
        ("git:github.com/user/repo", "https://github.com/user/repo", None),
        ("git:git@github.com:user/repo@v1.0.0", "git@github.com:user/repo", Some("v1.0.0")),
        ("git:https://github.com/user/repo", "https://github.com/user/repo", None),
        ("https://github.com/user/repo.git", "https://github.com/user/repo.git", None),
        ("https://github.com/user/repo@feature/branch", "https://github.com/user/repo", Some("feature/branch")),
        ("ssh://git@github.com/user/repo@v1", "ssh://git@github.com/user/repo", Some("v1")),
        ("https://github.com/user/repo/tree/main", "https://github.com/user/repo/tree/main", Some("main")),
        ("git:user/repo@v2", "https://github.com/user/repo", Some("v2")),
    ] {
        let (actual_repo, host, path, actual_ref) = git(source);
        let want = expected(repo, git_ref);
        assert_eq!((host, path, actual_ref, actual_repo), (want.1, want.2, want.3, want.4), "{source}");
    }
}

#[test]
fn other_hosts_parse_generically() {
    for (source, host) in [
        ("https://gitlab.com/user/repo", "gitlab.com"),
        ("https://bitbucket.org/user/repo", "bitbucket.org"),
        ("https://codeberg.org/user/repo", "codeberg.org"),
        ("https://www.github.com/user/repo", "github.com"),
    ] {
        let (_, actual_host, path, _) = git(source);
        assert_eq!((actual_host.as_str(), path.as_str()), (host, "user/repo"), "{source}");
    }
    assert_eq!(git("https://gitlab.com/group/sub/repo").2, "group/sub/repo");
    assert_eq!(git("git:localhost/team/repo").0, "https://localhost/team/repo");
}

#[test]
fn unsafe_and_unprefixed_inputs_are_rejected() {
    for source in [
        "git:git@evil.example:../../victim/repo",
        "https://evil.example/..%2F..%2Fvictim/repo",
        "https://evil.example/..%2F..%2Fvictim/repo%",
        "git:git@evil.example:/absolute/repo",
        "git:git@evil.example:user\\repo/name",
        "git:git@evil.example:user/repo\0name",
        "git@github.com:user/repo",
        "github.com/user/repo",
        "user/repo",
        "https://github.com/user",
    ] {
        assert!(parse_git_url(source).is_none(), "{source}");
    }
}

#[test]
fn npm_specs_pin_exact_versions() {
    let npm = |source: &str| match parse_source(source) {
        ParsedSource::Npm(npm) => npm,
        other => panic!("expected npm source for {source}: {other:?}"),
    };
    assert!(npm("npm:@scope/pkg@1.2.3").pinned);
    assert!(!npm("npm:@scope/pkg@^1.2.3").pinned);
    assert!(!npm("npm:pkg").pinned);
    assert!(!npm("npm:pkg@latest").pinned);
    let scoped = npm("npm:@scope/pkg@^1.2.3");
    assert_eq!((scoped.name.as_str(), scoped.version.as_deref()), ("@scope/pkg", Some("^1.2.3")));
    assert!(scoped.has_range && scoped.accepts("1.4.0") && !scoped.accepts("2.0.0"));
    let tag = parse_npm_source("pkg@beta");
    assert!(!tag.has_range && tag.accepts("9.9.9"));
    assert_eq!(parse_npm_source("@scope/pkg").name, "@scope/pkg");
}

#[test]
fn marketplace_sources_preserve_names_and_release_pins() {
    assert_eq!(parse_source("marketplace:todo"), ParsedSource::Marketplace { name: "todo".into(), tag: None });
    assert_eq!(
        parse_source("marketplace:todo@v1.2.3"),
        ParsedSource::Marketplace { name: "todo".into(), tag: Some("v1.2.3".into()) }
    );
    assert!(!parse_source("marketplace:todo").is_pinned());
    assert!(parse_source("marketplace:todo@v1.2.3").is_pinned());
    assert!(!crate::resources::paths::is_local_path("marketplace:todo"));
    assert!(crate::resources::paths::is_local_path("./marketplace:todo"));
    let fixture = Fixture::new();
    let manager = fixture.manager(MockRunner::failing());
    assert_eq!(manager.package_identity("marketplace:todo@v1.2.3", None).unwrap(), "marketplace:todo");
    manager.add_source_to_settings("marketplace:todo@v1.2.3", false).unwrap();
    manager.add_source_to_settings("marketplace:todo", false).unwrap();
    assert_eq!(
        fixture.settings.packages(crate::settings::SettingsScope::Global),
        sources(serde_json::json!(["marketplace:todo"]))
    );
    assert!(manager.remove_source_from_settings("marketplace:todo@v9", false).unwrap());
}

#[test]
fn sources_classify_like_the_docs() {
    let kind = |source: &str| match parse_source(source) {
        ParsedSource::Npm(_) => "npm",
        ParsedSource::Git(_) => "git",
        ParsedSource::Archive(_) => "archive",
        ParsedSource::Marketplace { .. } => "marketplace",
        ParsedSource::Local(_) => "local",
    };
    for source in
        ["git:github.com/user/repo@v1", "https://github.com/user/repo@v1", "ssh://git@github.com/user/repo@v1"]
    {
        assert_eq!(kind(source), "git", "{source}");
    }
    for source in ["/absolute/path", "./relative/path", "../relative/path", "github.com/user/repo", "./packages/a"] {
        assert_eq!(kind(source), "local", "{source}");
    }
    let ParsedSource::Archive(archive) = parse_source("https://example.com/dl/tools-1.0.0.tar.gz?x=1") else {
        panic!("expected archive");
    };
    assert_eq!((archive.host.as_str(), archive.path.as_str()), ("example.com", "dl/tools-1.0.0"));
    assert_eq!(archive.format, ArchiveFormat::TarGz);
    assert!(
        matches!(parse_source("http://127.0.0.1:8080/a/pkg.ZIP"), ParsedSource::Archive(a) if a.format == ArchiveFormat::Zip)
    );
    assert!(matches!(parse_source("https://example.com/pkg.tgz"), ParsedSource::Archive(_)));
}

#[test]
fn package_identity_ignores_versions_and_url_forms() {
    let fixture = Fixture::new();
    let manager = fixture.manager(MockRunner::failing());
    let identity = |source: &str| manager.package_identity(source, None).unwrap();
    for source in [
        "https://github.com/user/repo",
        "https://github.com/user/repo@v1.0.0",
        "git:github.com/user/repo",
        "https://github.com/user/repo.git",
        "git:git@github.com:user/repo",
        "ssh://git@github.com/user/repo",
    ] {
        assert_eq!(identity(source), "git:github.com/user/repo", "{source}");
    }
    assert_eq!(identity("npm:@scope/pkg@1.0.0"), "npm:@scope/pkg");
    assert_eq!(identity("https://example.com/a/pkg.tgz"), "url:example.com/a/pkg");
    let local = manager.package_identity("./pkg", Some(SourceScope::User)).unwrap();
    assert_eq!(local, format!("local:{}", fixture.agent_dir.join("pkg").display()));
}

#[test]
fn npm_command_detection_and_arguments() {
    let command = |argv: &[&str]| NpmCommand::from_argv(Some(&argv.iter().map(|a| a.to_string()).collect::<Vec<_>>()));
    let name = |argv: &[&str]| command(argv).unwrap().package_manager_name().unwrap();
    assert_eq!(name(&["npm", "exec", "--", "pnpm"]), "pnpm");
    assert_eq!(name(&["corepack", "pnpm"]), "pnpm");
    assert_eq!(name(&["mise", "exec", "bun@1", "--", "bun"]), "bun");
    assert_eq!(name(&["/opt/node/npm.CMD"]), "npm");
    assert_eq!(name(&["custom-tool"]), "custom-tool");
    assert!(command(&["wrapper", "npm", "pnpm"]).unwrap().package_manager_name().is_err());
    assert!(command(&[""]).is_err());
    assert_eq!(NpmCommand::from_argv(None).unwrap().command, "npm");
    let root = Path::new("/r");
    let specs = vec!["a".to_string()];
    assert_eq!(
        npm_install_args("npm", &specs, root),
        ["install", "a", "--prefix", "/r", "--legacy-peer-deps", "--no-audit", "--no-fund"]
    );
    assert_eq!(npm_install_args("bun", &specs, root), ["install", "a", "--cwd", "/r", "--omit=peer"]);
    assert_eq!(npm_install_args("pnpm", &specs, root)[4], "--config.auto-install-peers=false");
    assert_eq!(
        git_dependency_install_args("npm"),
        ["install", "--omit=dev", "--legacy-peer-deps", "--no-audit", "--no-fund"]
    );
    assert_eq!(git_dependency_install_args("pnpm")[1], "--prod");
    assert_eq!(git_dependency_install_args("yarn"), ["install"]);
}

#[test]
fn manifests_keep_string_arrays_and_gallery_fields() {
    let fixture = Fixture::new();
    let path = fixture.root().join("pkg").join("package.json");
    let package = serde_json::json!({
        "name": "pkg",
        "keywords": ["pi-package"],
        "pi": {"extensions": ["./a.ts"], "skills": ["s", 3], "prompts": "p", "image": "https://x/i.png"}
    });
    write(&path, &format!("\u{feff}{package}"));
    let manifest = read_pi_manifest(&path).unwrap();
    assert_eq!(manifest.extensions, Some(vec!["./a.ts".to_string()]));
    assert_eq!((manifest.skills, manifest.prompts, manifest.themes), (None, None, None));
    assert_eq!(manifest.image.as_deref(), Some("https://x/i.png"));
    let info = gallery_info(&package);
    assert!(info.listed && info.name.as_deref() == Some("pkg"));
    write(&path, "{\"name\": \"no-pi\"}");
    assert!(read_pi_manifest(&path).is_none());
    write(&path, "not json");
    assert!(read_pi_manifest(&path).is_none());
}

#[cfg(unix)]
#[test]
fn package_dir_honors_the_environment_override() {
    let dir = crate::packages::package_dir_from(Some("/nix/store/pi"), Some(Path::new("/usr/bin/pi")));
    assert_eq!(dir, PathBuf::from("/nix/store/pi"));
    let exe = crate::packages::package_dir_from(None, Some(Path::new("/opt/pi/bin/pi")));
    assert_eq!(exe, PathBuf::from("/opt/pi/bin"));
}
