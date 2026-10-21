//! Durable's `listFiles` conformance cases (upstream `env-conformance.ts`), for any env that lists through the daemon.

use pi_chord::{AbortController, Context, with_abort_signal};
use pi_durable::env::{ExecutionEnv, FileError, FileKind, FileResult};
use pi_env::{FileListing, ListFilesOptions, LocalExecutionEnv, RemoteExecutionEnv};

use super::ctx;

/// An env with `list_files`, which is not part of the `FileSystem` trait yet.
pub trait Lister: ExecutionEnv {
    async fn list(&self, path: &str, options: Option<&ListFilesOptions>, context: &Context) -> FileResult<FileListing>;
}

impl Lister for RemoteExecutionEnv {
    async fn list(&self, path: &str, options: Option<&ListFilesOptions>, context: &Context) -> FileResult<FileListing> {
        self.list_files(path, options, context).await
    }
}

impl Lister for LocalExecutionEnv {
    async fn list(&self, path: &str, options: Option<&ListFilesOptions>, context: &Context) -> FileResult<FileListing> {
        self.list_files(path, options, context).await
    }
}

fn aborted_context() -> Context {
    let controller = AbortController::new();
    controller.abort(None);
    with_abort_signal(&controller.signal(), &ctx())
}

/// A listing as `<kind letter> <path>` lines (`f`, `d`, `l`), with its truncation.
async fn listed(env: &impl Lister, path: &str, options: Option<ListFilesOptions>) -> (Vec<String>, bool) {
    let listing = env.list(path, options.as_ref(), &ctx()).await.expect("list");
    let letter = |kind| match kind {
        FileKind::File => "f",
        FileKind::Directory => "d",
        FileKind::Symlink => "l",
    };
    let entries = listing.entries.iter().map(|entry| format!("{} {}", letter(entry.kind), entry.path));
    (entries.collect(), listing.truncated)
}

async fn write_all(env: &impl Lister, files: &[&str]) {
    for file in files {
        env.write_file(file, b"", &ctx()).await.expect("write");
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

pub async fn lists_files_and_directories_depth_first_in_name_order(env: &impl Lister) {
    write_all(env, &["b.txt", "a/z.txt", "a/b/c.txt", ".hidden/x", "A.txt"]).await;
    env.create_dir("empty", false, &ctx()).await.expect("mkdir");
    let all = ["d .hidden", "f .hidden/x", "f A.txt", "d a", "d a/b", "f a/b/c.txt", "f a/z.txt", "f b.txt", "d empty"];
    assert_eq!(listed(env, ".", None).await, (strings(&all), false));
    let visible = ListFilesOptions { hidden: Some(false), ..Default::default() };
    assert_eq!(listed(env, ".", Some(visible)).await.0, strings(&all[2..]));
    let expected = (strings(&["d b", "f b/c.txt", "f z.txt"]), false);
    assert_eq!(listed(env, "a", None).await, expected);
    let absolute = env.absolute_path("a", &ctx()).await.expect("absolute");
    assert_eq!(listed(env, &absolute, None).await, expected);
    assert_eq!(listed(env, "empty", None).await, (Vec::new(), false));
}

pub async fn skips_what_git_ignores_in_work_trees_git_and_excluded_names(env: &impl Lister) {
    write_all(env, &["keep.txt", "out/x.js", "sub/keep.txt", "sub/drop.log", "node_modules/m/i.js"]).await;
    env.write_file(".gitignore", b"out/\n*.log\n", &ctx()).await.expect("write");
    env.write_file("sub/.gitignore", b"!keep.txt\n", &ctx()).await.expect("write");
    // Not a work tree yet: `.gitignore` files have no effect.
    assert!(listed(env, ".", None).await.0.contains(&"f out/x.js".to_string()));
    env.write_file(".git/HEAD", b"ref: refs/heads/main\n", &ctx()).await.expect("write");
    env.write_file(".git/info/exclude", b"keep.txt\n", &ctx()).await.expect("write");
    let excluded = ListFilesOptions { exclude: Some(strings(&["node_modules"])), ..Default::default() };
    let expected = strings(&["f .gitignore", "d sub", "f sub/.gitignore", "f sub/keep.txt"]);
    assert_eq!(listed(env, ".", Some(excluded)).await.0, expected);
    // A directory inside the work tree applies its parents' rules.
    assert_eq!(listed(env, "sub", None).await.0, strings(&["f .gitignore", "f keep.txt"]));
    let unfiltered = ListFilesOptions { gitignore: Some(false), ..Default::default() };
    let all = listed(env, ".", Some(unfiltered)).await.0;
    let has = |entry: &str| all.contains(&entry.to_string());
    assert!(has("f out/x.js") && has("f node_modules/m/i.js") && has("f keep.txt"));
    assert!(!all.iter().any(|entry| entry.contains(".git/") || entry == "d .git"));
}

pub async fn stops_at_its_bounds_and_reports_truncated(env: &impl Lister) {
    write_all(env, &["a", "b", "c"]).await;
    let entries = |max| Some(ListFilesOptions { max_entries: Some(max), ..Default::default() });
    assert_eq!(listed(env, ".", entries(2)).await, (strings(&["f a", "f b"]), true));
    assert_eq!(listed(env, ".", entries(3)).await, (strings(&["f a", "f b", "f c"]), false));
    // Each entry counts its path plus two bytes.
    let bytes = Some(ListFilesOptions { max_bytes: Some(6), ..Default::default() });
    assert_eq!(listed(env, ".", bytes).await, (strings(&["f a", "f b"]), true));
    assert_eq!(listed(env, ".", entries(0)).await, (Vec::new(), true));
    let unsafe_count = ListFilesOptions { timeout_ms: Some(u64::MAX), ..Default::default() };
    let invalid = env.list(".", Some(&unsafe_count), &ctx()).await.expect_err("invalid");
    assert_eq!(invalid.code.as_str(), "invalid");
}

pub async fn fails_on_missing_directories_files_and_aborts(env: &impl Lister) {
    env.write_file("file.txt", b"", &ctx()).await.expect("write");
    let code =
        |result: Result<FileListing, FileError>| result.map(|_| "ok").unwrap_or_else(|error| error.code.as_str());
    assert_eq!(code(env.list("missing", None, &ctx()).await), "not_found");
    assert_eq!(code(env.list("file.txt", None, &ctx()).await), "not_directory");
    assert_eq!(code(env.list(".", None, &aborted_context()).await), "aborted");
}

/// Needs a shell with a working `ln -s`.
pub async fn lists_symbolic_links_without_following_them(env: &impl Lister, shell: &[String]) {
    use pi_durable::env::{ShellCommand, ShellExecOptions};
    env.write_file("dir/inner.txt", b"", &ctx()).await.expect("write");
    let script = "ln -s dir link && ln -s missing dangling && ln -s .. dir/loop";
    let command = ShellCommand::Argv(shell.iter().cloned().chain([script.to_string()]).collect());
    let linked = env.exec(command, ShellExecOptions::default(), &ctx()).await.expect("exec");
    assert_eq!(linked.exit_code, 0);
    let expected = ["l dangling", "d dir", "f dir/inner.txt", "l dir/loop", "l link"];
    assert_eq!(listed(env, ".", None).await.0, strings(&expected));
    // The listed directory itself may be a link.
    assert_eq!(listed(env, "link", None).await.0, strings(&["f inner.txt", "l loop"]));
}
