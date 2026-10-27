//! Conformance cases for any `ExecutionEnv` (upstream `createEnvConformance`). Each case is a public async function
//! that panics on failure; `env_conformance_tests!` generates one `#[tokio::test]` per case.

mod exec;
mod files;
mod list_files;
mod watch;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::{AbortController, BACKGROUND_CONTEXT, Context, with_abort_signal};

pub use exec::*;
pub use files::*;
pub use list_files::*;
pub use watch::*;

use crate::env::{
    ExecutionError, FileChange, FileWatcher, OutputStream, SharedEnv, ShellCommand, ShellExecOptions, ShellExecResult,
    WatchTarget,
};

/// Options of the conformance cases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvConformanceOptions {
    /// Program and flag that run a POSIX shell script from the next argument; default `["sh", "-c"]`.
    pub shell: Vec<String>,
    /// Whether the shell's `ln -s` creates symbolic links; default true. Without it, symbolic link cases check nothing.
    pub symlinks: bool,
}

impl Default for EnvConformanceOptions {
    fn default() -> Self {
        Self { shell: vec!["sh".into(), "-c".into()], symlinks: true }
    }
}

/// One `#[tokio::test]` per case. `$setup` may `.await` and yields `(SharedEnv, guard)`: an env whose cwd is a fresh
/// empty directory, and a value dropped after the case. `$options` is an `EnvConformanceOptions`. Invoke it inside a
/// module of a crate that has `tokio` with `macros` and `rt-multi-thread`.
#[macro_export]
macro_rules! env_conformance_tests {
    ($setup:expr, $options:expr $(,)?) => {
        $crate::env_conformance_tests!(@cases $setup, $options;
            binary_reader_reads_byte_ranges_of_the_opened_file,
            binary_reader_scans_lines_like_decoding_the_whole_file,
            binary_reader_keeps_reading_the_file_it_opened_after_a_rename,
            binary_reader_refuses_directories_missing_files_and_aborted_opens,
            directory_reader_pages_every_entry_exactly_once,
            directory_reader_reports_the_end_and_refuses_use_after_close,
            directory_reader_refuses_missing_paths_and_files,
            directory_reader_skips_entries_removed_during_enumeration,
            watch_reports_a_missing_files_creation_changes_replacement_and_removal,
            watch_reports_a_missing_target_whose_ancestors_are_created,
            watch_follows_directories_created_together_with_their_contents,
            watch_keeps_watching_a_path_whose_parent_is_renamed_and_recreated,
            watch_skips_excluded_entries_and_reports_a_rename_out_of_them,
            watch_keeps_recursive_coverage_where_a_non_recursive_target_overlaps,
            watch_follows_a_directory_replaced_at_the_same_path,
            watch_stops_reporting_once_closed,
            argv_exec_passes_arguments_to_the_program_without_shell_parsing,
            exec_reports_the_stream_of_every_chunk_in_both_forms,
            argv_exec_honors_cwd_and_exit_codes,
            argv_exec_reports_missing_programs_and_empty_argv_as_spawn_errors,
            windowed_exec_keeps_the_exact_tail_and_counts_what_it_skips,
            argv_exec_distinguishes_timeout_from_abort,
            binary_reader_follows_symlinks_unless_no_follow_refuses_the_final_one,
            watch_reports_changes_to_the_file_a_watched_symbolic_link_points_to,
        );
    };
    (@cases $setup:expr, $options:expr; $($case:ident),* $(,)?) => {
        $(
            #[::tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn $case() {
                let (env, _guard) = $setup;
                let options: $crate::testing::env_conformance::EnvConformanceOptions = $options;
                $crate::testing::env_conformance::$case(env, &options).await;
            }
        )*
    };
}

/// The `list_files` cases, for an environment that implements it (upstream's `listFiles` option). Invoke it like
/// `env_conformance_tests!`; the symbolic link case checks nothing without `symlinks`.
#[macro_export]
macro_rules! env_list_files_conformance_tests {
    ($setup:expr, $options:expr $(,)?) => {
        $crate::env_conformance_tests!(@cases $setup, $options;
            list_files_lists_files_and_directories_depth_first_in_name_order,
            list_files_skips_what_git_ignores_in_work_trees_git_and_excluded_names,
            list_files_stops_at_its_bounds_and_reports_truncated,
            list_files_fails_on_missing_directories_files_and_aborts,
            list_files_lists_symbolic_links_without_following_them,
        );
    };
}

pub(crate) fn context() -> Context {
    BACKGROUND_CONTEXT.clone()
}

pub(crate) fn aborted_context() -> Context {
    let controller = AbortController::new();
    controller.abort(None);
    with_abort_signal(&controller.signal(), &BACKGROUND_CONTEXT)
}

/// `options.shell` followed by `rest`.
pub(crate) fn shell_argv(options: &EnvConformanceOptions, rest: &[&str]) -> ShellCommand {
    ShellCommand::Argv(options.shell.iter().cloned().chain(rest.iter().map(|part| part.to_string())).collect())
}

/// Output of a command, by stream.
pub(crate) struct Collected {
    pub result: Result<ShellExecResult, ExecutionError>,
    pub stdout: String,
    pub stderr: String,
}

pub(crate) async fn exec_collect(env: &SharedEnv, command: ShellCommand, cwd: Option<&str>) -> Collected {
    let output = Arc::new(Mutex::new((String::new(), String::new())));
    let sink = output.clone();
    let options = ShellExecOptions {
        cwd: cwd.map(str::to_string),
        on_output: Some(Arc::new(move |text, _, info| {
            let mut output = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match info.stream {
                OutputStream::Stdout => output.0.push_str(text),
                OutputStream::Stderr => output.1.push_str(text),
            }
        })),
        ..ShellExecOptions::default()
    };
    let result = env.exec(command, options, &context()).await;
    let (stdout, stderr) = output.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    Collected { result, stdout, stderr }
}

/// Whether a change reports `path`: an overflow, or a reported path at or above it.
fn covers(change: &FileChange, path: &str) -> bool {
    match change {
        FileChange::Overflow => true,
        FileChange::Paths(paths) => paths.iter().any(|reported| {
            path == reported || path.starts_with(&format!("{reported}/")) || path.starts_with(&format!("{reported}\\"))
        }),
        FileChange::Error(_) => false,
    }
}

/// A watch whose changes are recorded, with a helper that waits for a change reporting a path.
pub(crate) struct Watching {
    env: SharedEnv,
    pub changes: Arc<Mutex<Vec<FileChange>>>,
    watcher: Box<dyn FileWatcher>,
}

impl Watching {
    pub async fn start(env: &SharedEnv, targets: &[WatchTarget]) -> Self {
        let changes = Arc::new(Mutex::new(Vec::new()));
        let sink = changes.clone();
        let listener =
            Arc::new(move |change| sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(change));
        let watcher = env.watch(targets, listener, &context()).await.expect("watch");
        Self { env: env.clone(), changes, watcher }
    }

    pub async fn absolute(&self, path: &str) -> String {
        self.env.absolute_path(path, &context()).await.expect("absolute path")
    }

    pub fn recorded(&self) -> Vec<FileChange> {
        self.changes.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// Run `change`, then wait up to three seconds for a change after it that reports `path`.
    pub async fn expect_change(&self, path: &str, change: impl Future<Output = ()>) {
        let target = self.absolute(path).await;
        let from = self.recorded().len();
        change.await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let changes = self.recorded();
            if changes[from..].iter().any(|change| covers(change, &target)) {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "No change reported {target}; got {:?}", &changes[from..]);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn close(self) {
        self.watcher.close(&context()).await;
    }
}
