//! `@` search result handling with a stand-in `fd` script that prints fixed paths: scoring,
//! ordering, quoting, `.git` filtering, aborts, and delivery to the editor from the worker thread.
#![cfg(unix)]

mod autocomplete_common;
mod editor_common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use autocomplete_common::{block_on, suggest, values};
use pi_tui::autocomplete::{AbortController, AutocompleteProvider, CombinedAutocompleteProvider, SuggestionOptions};
use pi_tui::ui_loop;

/// Writes an executable script that prints `output`, or only sleeps `delay` seconds when set
/// (`exec`, so killing the process closes its output).
fn fake_fd(dir: &Path, output: &str, delay: u32) -> String {
    let data = dir.join("fd-output.txt");
    std::fs::write(&data, output).unwrap();
    let script = dir.join("fake-fd");
    let body = if delay > 0 { format!("exec sleep {delay}") } else { format!("cat '{}'", data.display()) };
    std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script.to_string_lossy().into_owned()
}

fn provider(base: &Path, fd: String) -> CombinedAutocompleteProvider {
    CombinedAutocompleteProvider::new(Vec::new(), base.to_string_lossy().into_owned(), Some(fd))
}

#[test]
fn scores_and_orders_fd_results() {
    let dir = tempfile::tempdir().unwrap();
    let fd = fake_fd(dir.path(), "src.txt\nsrc/\nREADME.md\nlib/src/\n.git/\n.git/config\na/.git/b\n", 0);
    let provider = provider(dir.path(), fd);
    let result = suggest(&provider, "@src", 4, false).expect("suggestions");
    assert_eq!(result.prefix, "@src");
    assert_eq!(values(&Some(result.clone())), ["@src/", "@lib/src/", "@src.txt"]);
    assert_eq!(result.items[0].label, "src/");
    assert_eq!(result.items[1].description.as_deref(), Some("lib/src"));

    let all = values(&suggest(&provider, "@", 1, false));
    // Equal scores: shallower first, then shorter.
    assert_eq!(all, ["@src/", "@src.txt", "@README.md", "@lib/src/"]);
}

#[test]
fn quotes_fd_paths_with_separators() {
    let dir = tempfile::tempdir().unwrap();
    let fd = fake_fd(dir.path(), "my folder/\nmy folder/test.txt\n", 0);
    let provider = provider(dir.path(), fd);
    assert_eq!(values(&suggest(&provider, "@my", 3, false)), ["@\"my folder/\"", "@\"my folder/test.txt\""]);
}

#[test]
fn without_fd_the_built_in_walker_suggests_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "x").unwrap();
    let provider = CombinedAutocompleteProvider::new(Vec::new(), dir.path().to_string_lossy().into_owned(), None);
    assert_eq!(values(&suggest(&provider, "@REA", 4, false)), ["@README.md"]);
}

#[test]
fn abort_kills_a_slow_fd() {
    let dir = tempfile::tempdir().unwrap();
    let fd = fake_fd(dir.path(), "README.md\n", 5);
    let provider = provider(dir.path(), fd);
    let controller = AbortController::new();
    let lines = vec!["@REA".to_string()];
    let pending =
        provider.get_suggestions(&lines, 0, 4, SuggestionOptions { signal: controller.signal(), force: false });
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    controller.abort();
    assert_eq!(block_on(pending), None);
    assert!(started.elapsed() < Duration::from_secs(3), "fd was not killed");
}

#[test]
fn editor_receives_fd_results_from_the_worker_thread() {
    let dir = tempfile::tempdir().unwrap();
    let fd = fake_fd(dir.path(), "README.md\nsrc/\n", 0);
    let editor = editor_common::editor();
    editor.set_autocomplete_provider(Rc::new(provider(dir.path(), fd)));
    editor_common::type_text(&editor, "@");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !editor.is_showing_autocomplete() && Instant::now() < deadline {
        ui_loop::run_once(Some(Duration::from_millis(20))).unwrap();
    }
    assert!(editor.is_showing_autocomplete());
    editor.handle_input("\t");
    assert_eq!(editor.get_text(), "@src/");
}
