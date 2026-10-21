//! `list_files` cases (upstream `createEnvConformance` with the `listFiles` option).

use super::{EnvConformanceOptions, aborted_context, context, shell_argv};
use crate::env::{FileErrorCode, FileKind, ListFilesOptions, SharedEnv};

/// A listing as `<kind letter> <path>` lines (`f`, `d`, `l`), with its truncation.
async fn listed(env: &SharedEnv, path: &str, options: Option<ListFilesOptions>) -> (Vec<String>, bool) {
    let listing = env.list_files(path, options.as_ref(), &context()).await.expect("list files");
    let letter = |kind| match kind {
        FileKind::File => 'f',
        FileKind::Directory => 'd',
        FileKind::Symlink => 'l',
    };
    let entries = listing.entries.iter().map(|entry| format!("{} {}", letter(entry.kind), entry.path)).collect();
    (entries, listing.truncated)
}

fn lines(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|entry| (*entry).to_owned()).collect()
}

/// Write empty files, creating the directories above them.
async fn write_all(env: &SharedEnv, files: &[&str]) {
    for file in files {
        write_text(env, file, "").await;
    }
}

async fn write_text(env: &SharedEnv, path: &str, text: &str) {
    if let Some((parent, _)) = path.rsplit_once('/') {
        env.create_dir(parent, true, &context()).await.expect("create directory");
    }
    env.write_file(path, text.as_bytes(), &context()).await.expect("write file");
}

fn code<T>(result: Result<T, crate::env::FileError>) -> Option<FileErrorCode> {
    result.err().map(|error| error.code)
}

pub async fn list_files_lists_files_and_directories_depth_first_in_name_order(
    env: SharedEnv,
    _options: &EnvConformanceOptions,
) {
    write_all(&env, &["b.txt", "a/z.txt", "a/b/c.txt", ".hidden/x", "A.txt"]).await;
    env.create_dir("empty", false, &context()).await.unwrap();
    let all = lines(&[
        "d .hidden",
        "f .hidden/x",
        "f A.txt",
        "d a",
        "d a/b",
        "f a/b/c.txt",
        "f a/z.txt",
        "f b.txt",
        "d empty",
    ]);
    assert_eq!(listed(&env, ".", None).await, (all, false));
    let visible = ListFilesOptions { hidden: Some(false), ..Default::default() };
    let expected = lines(&["f A.txt", "d a", "d a/b", "f a/b/c.txt", "f a/z.txt", "f b.txt", "d empty"]);
    assert_eq!(listed(&env, ".", Some(visible)).await.0, expected);
    let expected = (lines(&["d b", "f b/c.txt", "f z.txt"]), false);
    assert_eq!(listed(&env, "a", None).await, expected);
    let absolute = env.absolute_path("a", &context()).await.unwrap();
    assert_eq!(listed(&env, &absolute, None).await, expected);
    assert_eq!(listed(&env, "empty", None).await, (Vec::new(), false));
}

pub async fn list_files_skips_what_git_ignores_in_work_trees_git_and_excluded_names(
    env: SharedEnv,
    _options: &EnvConformanceOptions,
) {
    write_all(&env, &["keep.txt", "out/x.js", "sub/keep.txt", "sub/drop.log", "node_modules/m/i.js"]).await;
    write_text(&env, ".gitignore", "out/\n*.log\n").await;
    write_text(&env, "sub/.gitignore", "!keep.txt\n").await;
    // Not a work tree yet: `.gitignore` files have no effect.
    assert!(listed(&env, ".", None).await.0.contains(&"f out/x.js".to_owned()));
    write_text(&env, ".git/HEAD", "ref: refs/heads/main\n").await;
    write_text(&env, ".git/info/exclude", "keep.txt\n").await;
    let excluded = ListFilesOptions { exclude: Some(vec!["node_modules".into()]), ..Default::default() };
    let expected = lines(&["f .gitignore", "d sub", "f sub/.gitignore", "f sub/keep.txt"]);
    assert_eq!(listed(&env, ".", Some(excluded)).await.0, expected);
    // A directory inside the work tree applies its parents' rules.
    assert_eq!(listed(&env, "sub", None).await.0, lines(&["f .gitignore", "f keep.txt"]));
    let unfiltered = ListFilesOptions { gitignore: Some(false), ..Default::default() };
    let everything = listed(&env, ".", Some(unfiltered)).await.0;
    for entry in ["f out/x.js", "f node_modules/m/i.js", "f keep.txt"] {
        assert!(everything.contains(&entry.to_owned()), "{entry} in {everything:?}");
    }
    assert!(!everything.iter().any(|entry| entry.contains(".git/") || entry == "d .git"), "{everything:?}");
}

pub async fn list_files_stops_at_its_bounds_and_reports_truncated(env: SharedEnv, _options: &EnvConformanceOptions) {
    write_all(&env, &["a", "b", "c"]).await;
    let entries = |max_entries| Some(ListFilesOptions { max_entries: Some(max_entries), ..Default::default() });
    assert_eq!(listed(&env, ".", entries(2)).await, (lines(&["f a", "f b"]), true));
    assert_eq!(listed(&env, ".", entries(3)).await, (lines(&["f a", "f b", "f c"]), false));
    // Each entry counts its path plus two bytes.
    let bytes = Some(ListFilesOptions { max_bytes: Some(6), ..Default::default() });
    assert_eq!(listed(&env, ".", bytes).await, (lines(&["f a", "f b"]), true));
    assert_eq!(listed(&env, ".", entries(0)).await, (Vec::new(), true));
    // Values past 2^53 - 1 stand in for upstream's negative and fractional numbers, which `u64` cannot hold.
    let ctx = context();
    let invalid = |options: ListFilesOptions| async move { code(env.list_files(".", Some(&options), &ctx).await) };
    let too_many = ListFilesOptions { max_entries: Some(1 << 53), ..Default::default() };
    assert_eq!(invalid(too_many).await, Some(FileErrorCode::Invalid));
}

pub async fn list_files_fails_on_missing_directories_files_and_aborts(
    env: SharedEnv,
    _options: &EnvConformanceOptions,
) {
    write_text(&env, "file.txt", "").await;
    let ctx = context();
    assert_eq!(code(env.list_files("missing", None, &ctx).await), Some(FileErrorCode::NotFound));
    assert_eq!(code(env.list_files("file.txt", None, &ctx).await), Some(FileErrorCode::NotDirectory));
    assert_eq!(code(env.list_files(".", None, &aborted_context()).await), Some(FileErrorCode::Aborted));
}

pub async fn list_files_lists_symbolic_links_without_following_them(env: SharedEnv, options: &EnvConformanceOptions) {
    if !options.symlinks {
        return;
    }
    write_text(&env, "dir/inner.txt", "").await;
    let command = shell_argv(options, &["ln -s dir link && ln -s missing dangling && ln -s .. dir/loop"]);
    let linked = env.exec(command, Default::default(), &context()).await;
    assert_eq!(linked.unwrap().exit_code, 0);
    let expected = lines(&["l dangling", "d dir", "f dir/inner.txt", "l dir/loop", "l link"]);
    assert_eq!(listed(&env, ".", None).await.0, expected);
    // The listed directory itself may be a link.
    assert_eq!(listed(&env, "link", None).await.0, lines(&["f inner.txt", "l loop"]));
}
