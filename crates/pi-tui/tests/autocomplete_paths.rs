//! Ports of upstream `autocomplete.test.ts`: path prefix extraction, `./` paths, and quoted paths.

mod autocomplete_common;

use autocomplete_common::{apply, provider, setup_folder, suggest, values};
use pi_tui::autocomplete::CombinedAutocompleteProvider;

#[test]
fn extracts_root_from_hey_slash_when_forced() {
    let provider = CombinedAutocompleteProvider::new(Vec::new(), "/tmp", None);
    let result = suggest(&provider, "hey /", 5, true).expect("suggestions for /");
    assert_eq!(result.prefix, "/");
}

#[test]
fn extracts_slash_a_when_forced() {
    let provider = CombinedAutocompleteProvider::new(Vec::new(), "/tmp", None);
    if let Some(result) = suggest(&provider, "/A", 2, true) {
        assert_eq!(result.prefix, "/A");
    }
}

#[test]
fn does_not_trigger_for_slash_commands() {
    let provider = CombinedAutocompleteProvider::new(Vec::new(), "/tmp", None);
    assert_eq!(suggest(&provider, "/model", 6, true), None);
}

#[test]
fn triggers_for_absolute_paths_in_command_arguments() {
    let provider = CombinedAutocompleteProvider::new(Vec::new(), "/tmp", None);
    let result = suggest(&provider, "/command /", 10, true).expect("suggestions");
    assert_eq!(result.prefix, "/");
}

#[test]
fn completes_chinese_prefixes_after_separators_on_tab() {
    let base = tempfile::tempdir().unwrap();
    setup_folder(base.path(), &[], &[("说明.md", "file"), ("文档/说明.md", "nested file")]);
    let provider = provider(base.path());
    let mut completions = vec![
        ("说".to_string(), "说明.md".to_string()),
        ("文".to_string(), "文档/".to_string()),
        ("文档/说".to_string(), "文档/说明.md".to_string()),
        ("./文档/说".to_string(), "./文档/说明.md".to_string()),
    ];
    if cfg!(unix) {
        let base = base.path().to_string_lossy();
        completions.push((format!("{base}/文档/说"), format!("{base}/文档/说明.md")));
    }
    for separator in " \t\u{3000}\u{a0}\u{ff0c}\u{ff1a}\u{ff1b}\u{3002}\u{ff01}\u{ff1f}\u{ff08}\u{300c}\u{300a}".chars()
    {
        for (prefix, value) in &completions {
            let before = format!("查看𠮷{separator}");
            let line = format!("{before}{prefix} 后文");
            let cursor = before.len() + prefix.len();
            let result = suggest(&provider, &line, cursor, true).unwrap_or_else(|| panic!("{line:?}"));
            assert_eq!(result.prefix, *prefix);
            assert_eq!(values(&Some(result.clone())), std::slice::from_ref(value));
            let applied = apply(&provider, &line, cursor, &result.items[0], &result.prefix);
            assert_eq!(applied.lines[0], format!("{before}{value} 后文"));
            assert_eq!(applied.cursor_col, before.len() + value.len());
        }
    }
}

#[test]
fn unquoted_separators_are_boundaries_even_for_existing_paths() {
    let base = tempfile::tempdir().unwrap();
    setup_folder(base.path(), &[], &[("归档/说明.md", "other")]);
    let provider = provider(base.path());
    for separator in [" ", "\u{3000}", "\u{ff0c}", "\u{3002}"] {
        let directory = format!("资料{separator}归档");
        setup_folder(base.path(), &[], &[(&format!("{directory}/说明.md"), "archive")]);
        for marker in ["", "@"] {
            let line = format!("{marker}{directory}/说");
            let result = suggest(&provider, &line, line.len(), true).unwrap_or_else(|| panic!("{line:?}"));
            assert_eq!(result.prefix, "归档/说");
            assert_eq!(values(&Some(result)), ["归档/说明.md"]);
        }
        let quoted = format!("查看\u{ff0c}\"{directory}/说\"后文");
        let cursor = quoted.find("\"后文").unwrap();
        let result = suggest(&provider, &quoted, cursor, true).expect("quoted suggestions");
        assert_eq!(result.prefix, format!("\"{directory}/说"));
        assert_eq!(values(&Some(result.clone())), [format!("\"{directory}/说明.md\"")]);
        let applied = apply(&provider, &quoted, cursor, &result.items[0], &result.prefix);
        assert_eq!(applied.lines[0], format!("查看\u{ff0c}\"{directory}/说明.md\"后文"));
        let missing = format!("查看\u{ff0c}\"不存在{separator}归档/说");
        assert_eq!(suggest(&provider, &missing, missing.len(), true), None);
    }
}

#[test]
fn completes_an_empty_prefix_after_separators() {
    let base = tempfile::tempdir().unwrap();
    setup_folder(base.path(), &[], &[("说明.md", "text")]);
    let provider = provider(base.path());
    for separator in [" ", "\t", "\u{3000}", "\u{ff0c}", "\u{3002}"] {
        for force in [false, true] {
            let line = format!("查看{separator}");
            let result = suggest(&provider, &line, line.len(), force).unwrap_or_else(|| panic!("{line:?}"));
            assert_eq!(result.prefix, "");
            assert_eq!(values(&Some(result)), ["说明.md"]);
        }
    }
    assert_eq!(suggest(&provider, "", 0, false), None);
}

#[test]
fn completes_paths_after_opening_wrappers() {
    let base = tempfile::tempdir().unwrap();
    setup_folder(base.path(), &[], &[("src/main.ts", "x")]);
    let provider = provider(base.path());
    for wrapper in ["(", "[", "{", "<", "`", "((", "(`"] {
        for prefix in ["src/ma", "./src/ma"] {
            let before = format!("see {wrapper}");
            let line = format!("{before}{prefix}");
            let result = suggest(&provider, &line, line.len(), true).unwrap_or_else(|| panic!("{line:?}"));
            assert_eq!(result.prefix, prefix);
            let value = prefix.replace("src/ma", "src/main.ts");
            assert_eq!(values(&Some(result.clone())), std::slice::from_ref(&value));
            let applied = apply(&provider, &line, line.len(), &result.items[0], &result.prefix);
            assert_eq!(applied.lines[0], format!("{before}{value}"));
        }
    }
}

#[test]
fn completes_quoted_paths_after_opening_wrappers() {
    let base = tempfile::tempdir().unwrap();
    setup_folder(base.path(), &[], &[("my dir/main.ts", "x")]);
    let provider = provider(base.path());
    let line = "see (\"my dir/ma";
    let result = suggest(&provider, line, line.len(), true).expect("suggestions");
    assert_eq!(result.prefix, "\"my dir/ma");
    assert_eq!(values(&Some(result)), ["\"my dir/main.ts\""]);
}

#[test]
fn keeps_wrappers_closed_inside_the_path() {
    let base = tempfile::tempdir().unwrap();
    setup_folder(base.path(), &[], &[("[slug]/page.tsx", "x"), ("(group)/layout.tsx", "x")]);
    let provider = provider(base.path());
    for (prefix, value) in
        [("[slug]/pa", "[slug]/page.tsx"), ("(group)/la", "(group)/layout.tsx"), ("./[slug]/pa", "./[slug]/page.tsx")]
    {
        let line = format!("see {prefix}");
        let result = suggest(&provider, &line, line.len(), true).unwrap_or_else(|| panic!("{line:?}"));
        assert_eq!(result.prefix, prefix);
        assert_eq!(values(&Some(result)), [value]);
    }
}

#[test]
fn preserves_cjk_in_unprefixed_tab_completions() {
    let base = tempfile::tempdir().unwrap();
    setup_folder(base.path(), &[], &[("文档/说明.md", "text")]);
    let provider = provider(base.path());
    let line = "文档/说";
    let result = suggest(&provider, line, line.len(), true).expect("suggestions");
    assert_eq!(result.prefix, line);
    assert_eq!(values(&Some(result)), ["文档/说明.md"]);
}

#[test]
fn preserves_dot_slash_for_files_and_directories() {
    let base = tempfile::tempdir().unwrap();
    setup_folder(base.path(), &["src"], &[("update.sh", "#!/bin/bash"), ("utils.ts", ""), ("src/index.ts", "")]);
    let provider = provider(base.path());
    assert!(values(&suggest(&provider, "./up", 4, true)).contains(&"./update.sh".to_string()));
    assert!(values(&suggest(&provider, "./sr", 4, true)).contains(&"./src/".to_string()));
}

#[test]
fn quotes_paths_with_separators_for_direct_completion() {
    let base = tempfile::tempdir().unwrap();
    let provider = provider(base.path());
    for separator in [" ", "\u{3000}", "\u{ff0c}", "\u{3002}"] {
        let directory = format!("my{separator}folder");
        setup_folder(base.path(), &[], &[(&format!("{directory}/test.txt"), "content")]);
        let result = suggest(&provider, "my", 2, true).expect("suggestions");
        let quoted = format!("\"{directory}/\"");
        let item = result.items.iter().find(|item| item.value == quoted).unwrap_or_else(|| panic!("{directory}"));
        let applied = apply(&provider, "my", 2, item, &result.prefix);
        let continued = suggest(&provider, &applied.lines[0], applied.cursor_col, true).expect("continued");
        assert_eq!(continued.prefix, format!("\"{directory}/"));
        assert_eq!(values(&Some(continued)), [format!("\"{directory}/test.txt\"")]);
    }
}

#[test]
fn keeps_quoted_directories_before_files() {
    let base = tempfile::tempdir().unwrap();
    setup_folder(base.path(), &["z folder", "z\u{ff0c}folder"], &[("a.txt", "text")]);
    let provider = provider(base.path());
    let result = suggest(&provider, "", 0, true).expect("suggestions");
    let directories: Vec<bool> = result.items.iter().map(|item| item.label.ends_with('/')).collect();
    assert_eq!(directories, [true, true, false]);
}

#[test]
fn continues_completion_inside_quoted_paths() {
    let base = tempfile::tempdir().unwrap();
    setup_folder(base.path(), &[], &[("my folder/test.txt", ""), ("my folder/other.txt", "")]);
    let provider = provider(base.path());
    let line = "\"my folder/\"";
    let found = values(&suggest(&provider, line, line.len() - 1, true));
    assert!(found.contains(&"\"my folder/test.txt\"".to_string()));
    assert!(found.contains(&"\"my folder/other.txt\"".to_string()));
}

#[test]
fn applies_quoted_completion_without_a_second_quote() {
    let base = tempfile::tempdir().unwrap();
    setup_folder(base.path(), &[], &[("my folder/test.txt", "content")]);
    let provider = provider(base.path());
    let line = "\"my folder/te\"";
    let cursor = line.len() - 1;
    let result = suggest(&provider, line, cursor, true).expect("suggestions");
    let item = result.items.iter().find(|item| item.value == "\"my folder/test.txt\"").expect("test.txt");
    assert_eq!(apply(&provider, line, cursor, item, &result.prefix).lines[0], "\"my folder/test.txt\"");
}

#[test]
fn sorts_directories_first_then_by_name() {
    let base = tempfile::tempdir().unwrap();
    setup_folder(base.path(), &["beta", "Alpha"], &[("b.txt", ""), ("A.txt", ""), ("a.md", "")]);
    let provider = provider(base.path());
    let labels: Vec<String> =
        suggest(&provider, "", 0, true).expect("suggestions").items.into_iter().map(|item| item.label).collect();
    assert_eq!(labels, ["Alpha/", "beta/", "a.md", "A.txt", "b.txt"]);
}
