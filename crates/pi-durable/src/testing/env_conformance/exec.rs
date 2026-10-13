//! Shell execution cases.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::{AbortController, with_abort_signal};

use super::{EnvConformanceOptions, context, exec_collect, shell_argv};
use crate::env::{ExecutionErrorCode, SharedEnv, ShellCommand, ShellExecOptions, ShellOutputWindow};

pub async fn argv_exec_passes_arguments_to_the_program_without_shell_parsing(
    env: SharedEnv,
    options: &EnvConformanceOptions,
) {
    let hostile = "it's $(touch pwned) `touch pwned` *; touch pwned";
    let command = shell_argv(options, &[r#"printf "%s|%s" "$1" "$2""#, "argv0", hostile, "a b"]);
    let collected = exec_collect(&env, command, None).await;
    assert_eq!(collected.result.unwrap().exit_code, 0);
    assert_eq!(collected.stdout, format!("{hostile}|a b"));
    assert!(!env.exists("pwned", &context()).await.unwrap());
}

pub async fn exec_reports_the_stream_of_every_chunk_in_both_forms(env: SharedEnv, options: &EnvConformanceOptions) {
    let script = "printf out; printf err >&2; printf more";
    for command in [shell_argv(options, &[script]), ShellCommand::Script(script.into())] {
        let collected = exec_collect(&env, command, None).await;
        assert_eq!(collected.result.unwrap().exit_code, 0);
        assert_eq!((collected.stdout.as_str(), collected.stderr.as_str()), ("outmore", "err"));
    }
}

pub async fn argv_exec_honors_cwd_and_exit_codes(env: SharedEnv, options: &EnvConformanceOptions) {
    env.create_dir("sub", true, &context()).await.unwrap();
    let made = exec_collect(&env, shell_argv(options, &["printf x > made.txt; exit 3"]), Some("sub")).await;
    assert_eq!(made.result.unwrap().exit_code, 3);
    assert_eq!(env.read_text_file("sub/made.txt", &context()).await.unwrap(), "x");
}

pub async fn argv_exec_reports_missing_programs_and_empty_argv_as_spawn_errors(
    env: SharedEnv,
    _options: &EnvConformanceOptions,
) {
    let missing = ShellCommand::Argv(vec!["pi-durable-conformance-missing-program".into()]);
    let error = env.exec(missing, ShellExecOptions::default(), &context()).await.unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::SpawnError);
    let error = env.exec(ShellCommand::Argv(Vec::new()), ShellExecOptions::default(), &context()).await.unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::SpawnError);
}

#[derive(Default)]
struct WindowTally {
    bytes: u64,
    newlines: u64,
    tail: String,
}

pub async fn windowed_exec_keeps_the_exact_tail_and_counts_what_it_skips(
    env: SharedEnv,
    options: &EnvConformanceOptions,
) {
    let lines = 2000;
    let window = ShellOutputWindow { max_bytes: 200, max_lines: 5, min_interval_ms: 0.0, bytes_per_second: 1e9 };
    let tally = Arc::new(Mutex::new(WindowTally::default()));
    let sink = tally.clone();
    let on_output = Arc::new(move |text: &str, _: &pi_chord::Context, info: &crate::env::ShellOutputInfo| {
        let mut tally = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let newlines = text.matches('\n').count() as u64;
        if let Some(skipped) = info.skipped {
            tally.bytes += skipped.bytes;
            tally.newlines += skipped.newlines;
            assert!(
                text.len() as u64 > window.max_bytes || newlines > window.max_lines,
                "a skip is followed by more than the window"
            );
            tally.tail.clear();
        }
        tally.bytes += text.len() as u64;
        tally.newlines += newlines;
        tally.tail.push_str(text);
    });
    let script = format!("i=0; while [ $i -lt {lines} ]; do echo line-$i; i=$((i+1)); done");
    let exec = ShellExecOptions { window: Some(window), on_output: Some(on_output), ..ShellExecOptions::default() };
    let result = env.exec(shell_argv(options, &[&script]), exec, &context()).await;
    assert_eq!(result.unwrap().exit_code, 0);
    let expected: Vec<String> = (0..lines).map(|index| format!("line-{index}\n")).collect();
    let tally = tally.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(tally.bytes as usize, expected.concat().len());
    assert_eq!(tally.newlines, lines);
    let kept = expected[expected.len() - window.max_lines as usize..].concat();
    assert!(tally.tail.ends_with(&kept), "the delivered output ends with the tail");
}

pub async fn argv_exec_distinguishes_timeout_from_abort(env: SharedEnv, options: &EnvConformanceOptions) {
    let timed = ShellExecOptions { timeout: Some(0.1), ..ShellExecOptions::default() };
    let error = env.exec(shell_argv(options, &["sleep 2"]), timed, &context()).await.unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::Timeout);

    let controller = AbortController::new();
    let ctx = with_abort_signal(&controller.signal(), &context());
    let running = env.exec(shell_argv(options, &["sleep 2"]), ShellExecOptions::default(), &ctx);
    let abort = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        controller.abort(None);
    };
    let (result, ()) = tokio::join!(running, abort);
    assert_eq!(result.unwrap_err().code, ExecutionErrorCode::Aborted);
}
