//! Spill cases of `NodeExecutionEnv shell`, and upstream `test/env-node-spill.test.ts`.

use pi_durable::env::{
    ExecutionErrorCode, FileSystem, LocalEnvOptions, LocalExecutionEnv, Shell, ShellExecOptions, ShellSpillOptions,
};

use crate::shell::collect;
use crate::support::{context, join, local_env, path_string, remove_parent, temp_dir};

fn spill(after_bytes: u64, after_lines: u64) -> Option<ShellSpillOptions> {
    Some(ShellSpillOptions { after_bytes, after_lines })
}

#[tokio::test]
async fn does_not_create_a_spill_before_output_crosses_its_thresholds() {
    let dir = temp_dir();
    let options = ShellExecOptions { spill: spill(100, 10), ..Default::default() };
    let result = local_env(dir.path()).exec("printf short".into(), options, &context()).await.unwrap();
    assert_eq!(result.spill_path, None);
}

#[tokio::test]
async fn preserves_exact_raw_bytes_in_the_spill_while_streaming_decoded_text() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let options = ShellExecOptions { spill: spill(1, 10), ..Default::default() };
    let result = env.exec(r"printf 'f\200\000o'".into(), options, &context()).await.unwrap();
    let path = result.spill_path.unwrap();
    assert_eq!(env.read_binary_file(&path, &context()).await.unwrap(), [0x66, 0x80, 0x00, 0x6f]);
    remove_parent(&path);
}

#[tokio::test]
async fn reports_the_spill_of_a_command_that_times_out() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let options = ShellExecOptions { timeout: Some(0.3), spill: spill(10, 10), ..Default::default() };
    let error = env.exec("printf 12345678901234567890; sleep 5".into(), options, &context()).await.unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::Timeout);
    let path = error.spill_path.unwrap();
    assert_eq!(env.read_text_file(&path, &context()).await.unwrap(), "12345678901234567890");
    remove_parent(&path);
}

/// Upstream overrides `createTempFile` to return a path in a missing directory; here the temporary root is a file.
#[tokio::test]
async fn fails_rather_than_silently_losing_a_requested_spill() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("not-a-dir"), "x").unwrap();
    let temp_dir = Some(join(dir.path(), "not-a-dir"));
    let env = LocalExecutionEnv::new(LocalEnvOptions { temp_dir, ..LocalEnvOptions::new(path_string(dir.path())) });
    let options = ShellExecOptions { spill: spill(10, 10), ..Default::default() };
    let error = env.exec("printf 12345678901234567890".into(), options, &context()).await.unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::Unknown);
    assert!(error.message.contains("Failed to preserve complete shell output"), "{}", error.message);
}

#[tokio::test]
async fn preserves_complete_large_output_in_the_spill() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let options = ShellExecOptions { spill: spill(10, 10), ..Default::default() };
    let command = "head -c 500000 /dev/zero | tr '\\0' x";
    let result = env.exec(command.into(), options, &context()).await.unwrap();
    let path = result.spill_path.unwrap();
    assert_eq!(env.read_text_file(&path, &context()).await.unwrap().len(), 500_000);
    remove_parent(&path);
}

#[tokio::test]
async fn streams_every_line_and_spills_them_all_once_output_crosses_its_line_threshold() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let options = ShellExecOptions { spill: spill(1024 * 1024, 100), ..Default::default() };
    let command = "i=1; while [ $i -le 15000 ]; do echo line-$i; i=$((i+1)); done";
    let (result, output) = collect(&env, command, options, &context()).await;
    let expected: String = (1..=15000).map(|index| format!("line-{index}\n")).collect();
    assert_eq!(output, expected);
    let path = result.unwrap().spill_path.unwrap();
    let spilled = env.read_text_lines(&path, None, &context()).await.unwrap();
    assert_eq!((spilled.len(), spilled.last().map(String::as_str)), (15000, Some("line-15000")));
    remove_parent(&path);
}

/// `env-node-spill.test.ts` mocks a slow spill stream and checks that output a descendant writes after the exit grace
/// period is kept. Rust awaits each spill write before reading on, so the grace period restarts after a slow write;
/// this runs the same command and spill without the mock.
#[cfg(unix)]
#[tokio::test]
async fn keeps_inherited_stdio_output_and_spills_it() {
    use std::sync::{Arc, Mutex};
    let dir = temp_dir();
    let env = local_env(dir.path());
    let output = Arc::new(Mutex::new(String::new()));
    let sink = output.clone();
    let options = ShellExecOptions {
        spill: spill(10, 10),
        on_output: Some(Arc::new(move |text, _, _| sink.lock().unwrap().push_str(text))),
        ..Default::default()
    };
    let command = "printf '%020d' 0 | tr 0 a; (printf '%01000d' 0 | tr 0 b) &";
    let result = env.exec(command.into(), options, &context()).await.unwrap();
    assert_eq!(output.lock().unwrap().len(), 1020);
    let path = result.spill_path.unwrap();
    let spilled = env.read_text_file(&path, &context()).await.unwrap();
    assert_eq!(spilled.len(), 1020);
    assert!(spilled.starts_with(&"a".repeat(20)));
    remove_parent(&path);
}
