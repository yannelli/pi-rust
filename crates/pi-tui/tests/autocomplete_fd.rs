//! Ports of the upstream `fd @ file suggestions` tests. Like upstream, they are skipped when `fd`
//! is not on `PATH`; `autocomplete_fd_fake.rs` covers the result handling without it.

mod autocomplete_common;

use std::path::{Path, PathBuf};

use autocomplete_common::{apply, setup_folder, suggest, values};
use pi_tui::autocomplete::CombinedAutocompleteProvider;

fn fd_path() -> Option<String> {
    let command = if cfg!(windows) { "where" } else { "which" };
    let output = std::process::Command::new(command).arg("fd").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).lines().find(|line| !line.trim().is_empty()).map(|l| l.trim().to_string())
}

/// A temp root with `cwd` and `outside` directories, or `None` (skip) without `fd`.
struct Fixture {
    _root: tempfile::TempDir,
    base: PathBuf,
    outside: PathBuf,
    fd: String,
}

impl Fixture {
    fn new() -> Option<Self> {
        let Some(fd) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return None;
        };
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("cwd");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        Some(Self { _root: root, base, outside, fd })
    }

    fn provider_at(&self, base: &Path) -> CombinedAutocompleteProvider {
        CombinedAutocompleteProvider::new(Vec::new(), base.to_string_lossy().into_owned(), Some(self.fd.clone()))
    }

    fn provider(&self) -> CombinedAutocompleteProvider {
        self.provider_at(&self.base)
    }

    fn values(&self, line: &str) -> Vec<String> {
        values(&suggest(&self.provider(), line, line.len(), false))
    }
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

#[test]
fn returns_all_entries_for_an_empty_query() {
    let Some(f) = Fixture::new() else { return };
    setup_folder(&f.base, &["src"], &[("README.md", "readme")]);
    assert_eq!(sorted(f.values("@")), ["@README.md", "@src/"]);
}

#[test]
fn recognizes_at_after_cjk_punctuation() {
    let Some(f) = Fixture::new() else { return };
    setup_folder(&f.base, &[], &[("README.md", "readme")]);
    let provider = f.provider();
    let mut befores = vec!["查看\u{ff0c}".to_string(), "\u{3000}".to_string()];
    befores.extend("\u{ff0c}\u{ff0e}\u{ff1a}\u{3002}\u{3001}\u{300c}\u{300a}\u{3010}".chars().map(String::from));
    for before in befores {
        for force in [false, true] {
            let line = format!("{before}@REA");
            let result = suggest(&provider, &line, line.len(), force).unwrap_or_else(|| panic!("{line:?}"));
            assert_eq!(result.prefix, "@REA");
            assert_eq!(values(&Some(result.clone())), ["@README.md"]);
            let applied = apply(&provider, &line, line.len(), &result.items[0], &result.prefix);
            assert_eq!(applied.lines[0], format!("{before}@README.md "));
            assert_eq!(applied.cursor_col, applied.lines[0].len());
        }
    }
}

#[test]
fn recognizes_at_after_opening_wrappers() {
    let Some(f) = Fixture::new() else { return };
    setup_folder(&f.base, &[], &[("README.md", "readme")]);
    let provider = f.provider();
    for before in ["(", "see (", "[", "`", "<", "{"] {
        let line = format!("{before}@REA");
        let result = suggest(&provider, &line, line.len(), false).unwrap_or_else(|| panic!("{line:?}"));
        assert_eq!(result.prefix, "@REA");
        let applied = apply(&provider, &line, line.len(), &result.items[0], &result.prefix);
        assert_eq!(applied.lines[0], format!("{before}@README.md "));
    }
    assert_eq!(suggest(&provider, "foo(@REA", 8, false), None);
}

#[test]
fn keeps_cjk_and_embedded_at_in_paths() {
    let Some(f) = Fixture::new() else { return };
    setup_folder(&f.base, &[], &[("文档/说明.md", "text"), ("文档@备份/说明.md", "backup")]);
    for before in ["", "查看\u{ff0c}"] {
        for directory in ["文档", "文档@备份"] {
            let prefix = format!("@{directory}/说");
            let line = format!("{before}{prefix}");
            let result = suggest(&f.provider(), &line, line.len(), false).unwrap_or_else(|| panic!("{line:?}"));
            assert_eq!(result.prefix, prefix);
            assert_eq!(values(&Some(result)), [format!("@{directory}/说明.md")]);
        }
    }
}

#[test]
fn completes_quoted_cjk_attachments_after_prose() {
    let Some(f) = Fixture::new() else { return };
    for separator in [" ", "\u{3000}", "\u{ff0c}", "\u{3002}"] {
        let directory = format!("我的{separator}文档");
        setup_folder(&f.base, &[], &[(&format!("{directory}/说明.md"), "text"), ("文档/说明.md", "other")]);
        let line = format!("查看\u{ff1a}@\"{directory}/说\"后文");
        let cursor = line.find("\"后文").unwrap();
        let provider = f.provider();
        let result = suggest(&provider, &line, cursor, false).expect("suggestions");
        assert_eq!(result.prefix, format!("@\"{directory}/说"));
        assert_eq!(values(&Some(result.clone())), [format!("@\"{directory}/说明.md\"")]);
        let applied = apply(&provider, &line, cursor, &result.items[0], &result.prefix);
        assert_eq!(applied.lines[0], format!("查看\u{ff1a}@\"{directory}/说明.md\" 后文"));
        assert_eq!(applied.cursor_col, format!("查看\u{ff1a}@\"{directory}/说明.md\" ").len());
    }
}

#[test]
fn ignores_email_addresses_and_at_after_letters() {
    let Some(f) = Fixture::new() else { return };
    setup_folder(&f.base, &[], &[("README.md", "readme"), ("example.com", "text")]);
    for before in ["user", "查看", "あ", "한", "𠮷", "Ａ"] {
        for name in ["REA", "example.com"] {
            let line = format!("{before}@{name}");
            assert_eq!(suggest(&f.provider(), &line, line.len(), false), None, "{line:?}");
        }
    }
}

#[test]
fn matches_extensions_case_insensitively_and_ranks_directories_first() {
    let Some(f) = Fixture::new() else { return };
    setup_folder(&f.base, &["src"], &[("file.txt", "content"), ("README.md", "readme"), ("src.txt", "text")]);
    assert!(f.values("@file.txt").contains(&"@file.txt".to_string()));
    assert_eq!(f.values("@re"), ["@README.md"]);
    let src = f.values("@src");
    assert_eq!(src[0], "@src/");
    assert!(src.contains(&"@src.txt".to_string()));
}

#[test]
fn returns_nested_and_deep_paths() {
    let Some(f) = Fixture::new() else { return };
    setup_folder(
        &f.base,
        &[],
        &[
            ("src/index.ts", ""),
            ("packages/tui/src/autocomplete.ts", ""),
            ("packages/ai/src/autocomplete.ts", ""),
            ("src/components/Button.tsx", ""),
            ("src/utils/helpers.ts", ""),
        ],
    );
    assert!(f.values("@index").contains(&"@src/index.ts".to_string()));
    let deep = f.values("@tui/src/auto");
    assert!(deep.contains(&"@packages/tui/src/autocomplete.ts".to_string()));
    assert!(!deep.contains(&"@packages/ai/src/autocomplete.ts".to_string()));
    let middle = f.values("@components/");
    assert!(middle.contains(&"@src/components/Button.tsx".to_string()));
    assert!(!middle.contains(&"@src/utils/helpers.ts".to_string()));
}

#[test]
fn scopes_search_to_relative_directories() {
    let Some(f) = Fixture::new() else { return };
    setup_folder(
        &f.outside,
        &[],
        &[("nested/alpha.ts", ""), ("nested/deeper/also-alpha.ts", ""), ("nested/deeper/zzz.ts", "")],
    );
    let found = f.values("@../outside/a");
    assert!(found.contains(&"@../outside/nested/alpha.ts".to_string()));
    assert!(found.contains(&"@../outside/nested/deeper/also-alpha.ts".to_string()));
    assert!(!found.contains(&"@../outside/nested/deeper/zzz.ts".to_string()));
}

#[test]
fn ranks_shallow_matches_first_even_when_flooded() {
    let Some(f) = Fixture::new() else { return };
    let mut dirs = vec!["scope/projects".to_string()];
    dirs.extend((1..=250).map(|i| format!("scope/a{i:03}/venv/lib/python3.12/site-packages/pkg/core/profile")));
    let dirs: Vec<&str> = dirs.iter().map(String::as_str).collect();
    setup_folder(&f.base, &dirs, &[]);
    let found = f.values("@scope/pro");
    assert_eq!(found[0], "@scope/projects/");
    assert!(found.iter().any(|value| value.contains("/profile/")));
}

#[test]
fn quotes_at_paths_with_separators() {
    let Some(f) = Fixture::new() else { return };
    let provider = f.provider();
    for separator in [" ", "\u{3000}", "\u{ff0c}", "\u{3002}"] {
        let directory = format!("my{separator}folder");
        setup_folder(&f.base, &[], &[(&format!("{directory}/test.txt"), "content")]);
        let result = suggest(&provider, "@my", 3, false).expect("suggestions");
        let quoted = format!("@\"{directory}/\"");
        let item = result.items.iter().find(|item| item.value == quoted).unwrap_or_else(|| panic!("{directory}"));
        let applied = apply(&provider, "@my", 3, item, &result.prefix);
        let continued = suggest(&provider, &applied.lines[0], applied.cursor_col, false).expect("continued");
        assert_eq!(continued.prefix, format!("@\"{directory}/"));
        assert!(values(&Some(continued)).contains(&format!("@\"{directory}/test.txt\"")));
    }
}

#[test]
fn includes_hidden_paths_but_not_git() {
    let Some(f) = Fixture::new() else { return };
    setup_folder(
        &f.base,
        &[".pi", ".github", ".git"],
        &[(".pi/config.json", "{}"), (".github/workflows/ci.yml", ""), (".git/config", "")],
    );
    let found = f.values("@");
    assert!(found.contains(&"@.pi/".to_string()));
    assert!(found.contains(&"@.github/".to_string()));
    assert!(!found.iter().any(|value| value == "@.git" || value.starts_with("@.git/")));
}

#[cfg(unix)]
#[test]
fn follows_symlinks() {
    let Some(f) = Fixture::new() else { return };
    setup_folder(&f.base, &[], &[("dir/some_file.txt", "real"), ("original.txt", "content")]);
    setup_folder(&f.outside, &[], &[("some_file.txt", "linked"), ("nested/file.txt", "")]);
    std::os::unix::fs::symlink("../outside", f.base.join("symlinked_dir")).unwrap();
    std::os::unix::fs::symlink("original.txt", f.base.join("link.txt")).unwrap();
    let some = f.values("@some");
    assert!(some.contains(&"@dir/some_file.txt".to_string()));
    assert!(some.contains(&"@symlinked_dir/some_file.txt".to_string()));
    assert!(f.values("@symlinked").contains(&"@symlinked_dir/".to_string()));
    assert!(f.values("@link").contains(&"@link.txt".to_string()));
}

#[test]
fn ignores_the_query_inside_the_cwd_path() {
    let Some(f) = Fixture::new() else { return };
    let normal = f.base.join("cwd-normal");
    let repro = f.base.join("cwd-plan-repro");
    for base in [&normal, &repro] {
        setup_folder(
            base,
            &["packages/coding-agent/examples/extensions/plan-mode"],
            &[("packages/coding-agent/examples/extensions/plan-mode/README.md", ""), ("packages/tui/docs/plan.md", "")],
        );
    }
    let describe = |base: &Path| {
        let result = suggest(&f.provider_at(base), "@plan", 5, false);
        let mut items: Vec<String> = result
            .map(|r| {
                r.items.into_iter().map(|i| format!("{} :: {}", i.label, i.description.unwrap_or_default())).collect()
            })
            .unwrap_or_default();
        items.sort();
        items
    };
    let normal_items = describe(&normal);
    assert_eq!(describe(&repro), normal_items);
    assert!(normal_items.contains(&"plan-mode/ :: packages/coding-agent/examples/extensions/plan-mode".to_string()));
    assert!(normal_items.contains(&"plan.md :: packages/tui/docs/plan.md".to_string()));
}

#[test]
fn continues_and_applies_inside_quoted_at_paths() {
    let Some(f) = Fixture::new() else { return };
    setup_folder(&f.base, &[], &[("my folder/test.txt", ""), ("my folder/other.txt", "")]);
    let provider = f.provider();
    let line = "@\"my folder/\"";
    let found = values(&suggest(&provider, line, line.len() - 1, false));
    assert!(found.contains(&"@\"my folder/test.txt\"".to_string()));
    assert!(found.contains(&"@\"my folder/other.txt\"".to_string()));

    let line = "@\"my folder/te\"";
    let result = suggest(&provider, line, line.len() - 1, false).expect("suggestions");
    let item = result.items.iter().find(|i| i.value == "@\"my folder/test.txt\"").expect("test.txt");
    assert_eq!(apply(&provider, line, line.len() - 1, item, &result.prefix).lines[0], "@\"my folder/test.txt\" ");
}
