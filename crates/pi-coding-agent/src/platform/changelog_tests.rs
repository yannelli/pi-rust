use super::*;

const UPSTREAM: ChangelogRepo = ChangelogRepo {
    github_repo: "earendil-works/pi",
    link_base_path: "packages/coding-agent",
    legacy_repo_urls: &["https://github.com/badlogic/pi-mono", "https://github.com/earendil-works/pi-mono"],
};

const SAMPLE: &str = "# Changelog\n\nIntro text.\n\n## [Unreleased]\n\n- pending\n\n## [0.3.1] - 2026-10-20\n\n### Fixed\n\n- bug\n\n## 0.3.0\n\n- feature\n\n## [0.2.10] - 2026-09-01\r\n\r\n- old\n";

fn versions(entries: &[ChangelogEntry]) -> Vec<String> {
    entries.iter().map(ChangelogEntry::version).collect()
}

#[test]
fn parses_versioned_sections_and_skips_others() {
    let entries = parse_changelog(SAMPLE);
    assert_eq!(versions(&entries), ["0.3.1", "0.3.0", "0.2.10"]);
    assert_eq!(entries[0].content, "## [0.3.1] - 2026-10-20\n\n### Fixed\n\n- bug");
    assert_eq!(entries[1].content, "## 0.3.0\n\n- feature");
    assert_eq!(entries[2].content, "## [0.2.10] - 2026-09-01\r\n\r\n- old");
    assert!(parse_changelog("no headers").is_empty());
}

#[test]
fn embedded_changelog_parses() {
    let entries = embedded_changelog_entries();
    assert!(!entries.is_empty());
    assert!(entries[0].content.starts_with("## "));
}

#[test]
fn new_entries_compare_numerically() {
    let entries = parse_changelog(SAMPLE);
    assert_eq!(versions(&get_new_entries(&entries, "0.2.9")), ["0.3.1", "0.3.0", "0.2.10"]);
    assert_eq!(versions(&get_new_entries(&entries, "0.3.0")), ["0.3.1"]);
    assert!(get_new_entries(&entries, "0.3.1").is_empty());
    assert_eq!(versions(&get_new_entries(&entries, "0.3.1-beta")), ["0.3.1"]);
}

#[test]
fn startup_records_fresh_installs_and_shows_updates() {
    let entries = parse_changelog(SAMPLE);
    let fresh = startup_changelog(&entries, None, false);
    assert_eq!(fresh, StartupChangelog { markdown: None, record_version: true, report_install: true });
    assert_eq!(startup_changelog(&entries, Some("0.3.1"), false), StartupChangelog::default());
    assert_eq!(startup_changelog(&entries, Some("0.1.0"), true), StartupChangelog::default());
    let update = startup_changelog(&entries, Some("0.3.0"), false);
    assert!(update.record_version && update.report_install);
    assert_eq!(update.markdown.as_deref(), Some(entries[0].content.as_str()));
}

#[test]
fn condensed_notice_and_full_view() {
    let entries = parse_changelog(SAMPLE);
    let markdown = startup_changelog(&entries, Some("0.2.0"), false).markdown.unwrap();
    assert_eq!(
        condensed_changelog_notice(&markdown, "9.9.9"),
        "Updated to v0.3.1. Use /changelog to view full changelog."
    );
    assert_eq!(condensed_changelog_version("no header", "1.2.3"), "1.2.3");
    let full = changelog_command_markdown(&entries);
    assert!(full.starts_with("## [0.2.10]") && full.ends_with("- bug"), "{full}");
    assert_eq!(changelog_command_markdown(&[]), "No changelog entries found.");
}

#[test]
fn rewrites_package_relative_links_to_tag_pinned_github_links() {
    let markdown = [
        "[Project Trust](README.md#project-trust)",
        "[Extensions](docs/extensions.md#project_trust)",
        "[Examples](examples/extensions/)",
        "[Root README](../../README.md#supply-chain-hardening)",
    ]
    .join("\n");
    let expected = [
        "[Project Trust](https://github.com/earendil-works/pi/blob/v0.79.0/packages/coding-agent/README.md#project-trust)",
        "[Extensions](https://github.com/earendil-works/pi/blob/v0.79.0/packages/coding-agent/docs/extensions.md#project_trust)",
        "[Examples](https://github.com/earendil-works/pi/tree/v0.79.0/packages/coding-agent/examples/extensions/)",
        "[Root README](https://github.com/earendil-works/pi/blob/v0.79.0/README.md#supply-chain-hardening)",
    ]
    .join("\n");
    assert_eq!(normalize_changelog_links_in(&markdown, "0.79.0", &UPSTREAM), expected);
}

#[test]
fn canonicalizes_old_repository_urls_without_changing_external_links() {
    let markdown = [
        "[#5167](https://github.com/earendil-works/pi-mono/pull/5167)",
        "[#4163](https://github.com/badlogic/pi-mono/issues/4163)",
        "[Agent README](https://github.com/badlogic/pi-mono/blob/main/packages/agent/README.md)",
        "[External](https://example.com/docs)",
        "[Local anchor](#settings)",
    ]
    .join("\n");
    let expected = [
        "[#5167](https://github.com/earendil-works/pi/pull/5167)",
        "[#4163](https://github.com/earendil-works/pi/issues/4163)",
        "[Agent README](https://github.com/earendil-works/pi/blob/v0.79.0/packages/agent/README.md)",
        "[External](https://example.com/docs)",
        "[Local anchor](#settings)",
    ]
    .join("\n");
    assert_eq!(normalize_changelog_links_in(&markdown, "0.79.0", &UPSTREAM), expected);
}

#[test]
fn pi_rust_links_resolve_from_the_repository_root() {
    let markdown = "![logo](assets/) [up](../x.md) [commit](https://github.com/yannelli/pi-rust/blob/main/src/lib.rs)";
    assert_eq!(
        normalize_changelog_links(markdown, "v0.2.0"),
        "![logo](https://github.com/yannelli/pi-rust/tree/v0.2.0/assets/) [up](../x.md) \
         [commit](https://github.com/yannelli/pi-rust/blob/v0.2.0/src/lib.rs)"
    );
    assert_eq!(
        normalize_changelog_links("[Docs](docs/caf\u{e9}.md?plain=1)", "0.2.0"),
        "[Docs](https://github.com/yannelli/pi-rust/blob/v0.2.0/docs/caf%C3%A9.md?plain=1)"
    );
}
