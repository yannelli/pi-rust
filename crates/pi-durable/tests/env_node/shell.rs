//! `NodeExecutionEnv shell` cases. Upstream runs `node -e` scripts for raw output; these use `printf` instead.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::Context;
use pi_durable::env::{
    ExecutionError, ExecutionErrorCode, FileSystem, LocalEnvOptions, LocalExecutionEnv, Shell, ShellExecOptions,
    ShellExecResult,
};

use crate::support::{abortable, aborted_context, context, join, local_env, path_string, temp_dir, wait_for};

pub async fn collect(
    env: &LocalExecutionEnv,
    command: &str,
    mut options: ShellExecOptions,
    ctx: &Context,
) -> (Result<ShellExecResult, ExecutionError>, String) {
    let output = Arc::new(Mutex::new(String::new()));
    let sink = output.clone();
    options.on_output = Some(Arc::new(move |text, _, _| sink.lock().unwrap().push_str(text)));
    let result = env.exec(command.into(), options, ctx).await;
    let text = output.lock().unwrap().clone();
    (result, text)
}

fn env_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect()
}

fn options_in(dir: &std::path::Path) -> LocalEnvOptions {
    LocalEnvOptions::new(path_string(dir))
}

#[tokio::test]
async fn executes_commands_in_cwd_with_env_overrides() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let options = ShellExecOptions { env: Some(env_map(&[("NODE_ENV_TEST", "ok")])), ..Default::default() };
    let command = r#"printf '%s' "$NODE_ENV_TEST" > cwd-marker.txt; printf '%s:%s' "$PWD" "$NODE_ENV_TEST""#;
    let (result, output) = collect(&env, command, options, &context()).await;
    assert_eq!(result.unwrap().exit_code, 0);
    assert_eq!(std::fs::read_to_string(dir.path().join("cwd-marker.txt")).unwrap(), "ok");
    if cfg!(unix) {
        let real = path_string(&std::fs::canonicalize(dir.path()).unwrap());
        assert_eq!(output, format!("{real}:ok"));
    }
}

#[tokio::test]
async fn applies_string_shell_environment_overrides() {
    let cases: [(Option<BTreeMap<String, String>>, &str); 3] = [
        // A missing override preserves the base value.
        (None, "x:/stale/parent.jsonl"),
        // An empty override shadows the base value.
        (Some(env_map(&[("PI_SESSION_FILE", "")])), "x:"),
        (Some(env_map(&[("PI_SESSION_FILE", "/sessions/current.jsonl")])), "x:/sessions/current.jsonl"),
    ];
    for (overrides, expected) in cases {
        let dir = temp_dir();
        let shell_env = env_map(&[
            ("PI_SESSION_FILE", "/stale/parent.jsonl"),
            ("PI_CODING_AGENT", "true"),
            ("PI_NODE_ENV_PRESERVED_TEST", "preserved"),
        ]);
        let env = LocalExecutionEnv::new(LocalEnvOptions { shell_env: Some(shell_env), ..options_in(dir.path()) });
        let command = r#"printf '%s:%s|%s|%s' "${PI_SESSION_FILE+x}" "${PI_SESSION_FILE-}" "$PI_CODING_AGENT" "$PI_NODE_ENV_PRESERVED_TEST""#;
        let options = ShellExecOptions { env: overrides, ..Default::default() };
        let (result, output) = collect(&env, command, options, &context()).await;
        result.unwrap();
        assert_eq!(output, format!("{expected}|true|preserved"));
    }
}

/// Upstream sets a variable in its own process first; `set_var` is unsafe, so this uses one cargo always sets.
#[tokio::test]
async fn can_replace_rather_than_inherit_the_default_shell_environment() {
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
    let dir = temp_dir();
    let shell_env = env_map(&[("PI_NODE_ENV_CONFIGURED_TEST", "configured")]);
    let env = LocalExecutionEnv::new(LocalEnvOptions { shell_env: Some(shell_env), ..options_in(dir.path()) });
    let options = ShellExecOptions {
        inherit_env: Some(false),
        env: Some(env_map(&[("PI_NODE_ENV_EXPLICIT_TEST", "explicit")])),
        ..Default::default()
    };
    let command = r#"printf '%s:%s:%s' "${CARGO_MANIFEST_DIR-}" "${PI_NODE_ENV_CONFIGURED_TEST-}" "${PI_NODE_ENV_EXPLICIT_TEST-}""#;
    let (result, output) = collect(&env, command, options, &context()).await;
    result.unwrap();
    assert_eq!(output, "::explicit");
}

/// Upstream also fakes `process.platform`; the legacy WSL path check does not depend on the platform here. The relative
/// shell path resolves against the process directory, which only this test changes.
#[cfg(unix)]
#[tokio::test]
async fn uses_stdin_command_transport_for_legacy_wsl_bash_paths() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir();
    let shell_path = "C:\\Windows\\System32\\bash.exe";
    let script = dir.path().join(shell_path);
    std::fs::write(&script, "#!/bin/sh\nprintf 'args:%s\\n' \"$*\" >&2\nexec /bin/bash \"$@\"\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let env = LocalExecutionEnv::new(LocalEnvOptions { shell_path: Some(shell_path.into()), ..options_in(dir.path()) });
    let command = "name='World'; echo \"Hello, ${name}!\"";
    // A script just written can fail with ETXTBSY while another test forks; retry the spawn.
    let mut attempt = 0;
    let (result, output) = loop {
        let (result, output) = collect(&env, command, ShellExecOptions::default(), &context()).await;
        attempt += 1;
        match &result {
            Err(error) if error.code == ExecutionErrorCode::SpawnError && attempt < 20 => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            _ => break (result, output),
        }
    };
    std::env::set_current_dir(original).unwrap();
    assert_eq!(result.unwrap().exit_code, 0);
    assert!(output.contains("Hello, World!"), "{output}");
    assert!(output.contains("args:-s"), "{output}");
}

// "settles after the shell exits when a detached descendant keeps inherited stdio open" runs only on Windows upstream
// and spawns Node; the exit grace period it checks is the code path of the last spill test in `spill.rs`.

#[tokio::test]
async fn cleanup_terminates_active_shell_processes() {
    let dir = temp_dir();
    let env = Arc::new(local_env(dir.path()));
    let running = env.clone();
    let execution =
        tokio::spawn(
            async move { running.exec("touch started; sleep 60".into(), Default::default(), &context()).await },
        );
    let probe = env.clone();
    wait_for(async || probe.exists("started", &context()).await.unwrap(), Duration::from_secs(10)).await;
    env.cleanup(&context()).await;
    let result = tokio::time::timeout(Duration::from_secs(3), execution).await.unwrap().unwrap();
    assert!(result.is_ok());
}

#[tokio::test]
async fn streams_combined_stdout_and_stderr() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let (result, output) = collect(&env, "printf out; printf err >&2", Default::default(), &context()).await;
    assert_eq!(result.unwrap(), ShellExecResult { exit_code: 0, spill_path: None });
    assert!(output.contains("out") && output.contains("err"));
}

#[tokio::test]
async fn decodes_utf8_split_across_raw_process_chunks() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let command = r"printf '\360\237'; sleep 0.05; printf '\230\200'";
    let (result, output) = collect(&env, command, Default::default(), &context()).await;
    result.unwrap();
    assert_eq!(output, "😀");
}

#[tokio::test]
async fn reports_a_missing_working_directory_before_spawning() {
    let dir = temp_dir();
    let env = local_env(&dir.path().join("missing"));
    let error = env.exec("printf ok".into(), Default::default(), &context()).await.unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::SpawnError);
    assert!(error.message.contains("Working directory does not exist"));
}

#[tokio::test]
async fn returns_non_zero_command_exit_codes_as_successful_execution_results() {
    let dir = temp_dir();
    let result = local_env(dir.path()).exec("exit 7".into(), Default::default(), &context()).await.unwrap();
    assert_eq!(result, ShellExecResult { exit_code: 7, spill_path: None });
}

#[cfg(unix)]
#[tokio::test]
async fn maps_signal_killed_processes_to_a_non_zero_exit_code() {
    let dir = temp_dir();
    let result = local_env(dir.path()).exec("kill -9 $$".into(), Default::default(), &context()).await.unwrap();
    assert_eq!(result.exit_code, 128 + 9);
}

#[tokio::test]
async fn returns_timeout_errors_for_commands_exceeding_the_timeout() {
    let dir = temp_dir();
    let options = ShellExecOptions { timeout: Some(0.01), ..Default::default() };
    let error = local_env(dir.path()).exec("sleep 5".into(), options, &context()).await.unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::Timeout);
}

#[tokio::test]
async fn rejects_invalid_timeouts_before_spawning() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    for timeout in [0.0, -1.0, f64::NAN, f64::INFINITY, 2_147_484.0] {
        let options = ShellExecOptions { timeout: Some(timeout), ..Default::default() };
        let error = env.exec("touch spawned".into(), options, &context()).await.unwrap_err();
        assert_eq!(error.code, ExecutionErrorCode::Timeout);
        assert!(error.message.contains("Invalid timeout"));
    }
    assert!(!env.exists("spawned", &context()).await.unwrap());
}

#[tokio::test]
async fn returns_callback_errors_from_exec_stream_handlers() {
    let dir = temp_dir();
    let options =
        ShellExecOptions { on_output: Some(Arc::new(|_, _, _| panic!("callback failed"))), ..Default::default() };
    let error = local_env(dir.path()).exec("printf out".into(), options, &context()).await.unwrap_err();
    assert_eq!((error.code, error.message.as_str()), (ExecutionErrorCode::CallbackError, "callback failed"));
}

#[tokio::test]
async fn returns_shell_unavailable_and_spawn_errors() {
    let dir = temp_dir();
    let missing = LocalEnvOptions { shell_path: Some(join(dir.path(), "missing-shell")), ..options_in(dir.path()) };
    let error = LocalExecutionEnv::new(missing).exec("printf ok".into(), Default::default(), &context()).await;
    assert_eq!(error.unwrap_err().code, ExecutionErrorCode::ShellUnavailable);

    let shell_path = join(dir.path(), "not-executable-shell");
    std::fs::write(&shell_path, "not executable").unwrap();
    let options = LocalEnvOptions { shell_path: Some(shell_path), ..options_in(dir.path()) };
    let error = LocalExecutionEnv::new(options).exec("printf ok".into(), Default::default(), &context()).await;
    assert_eq!(error.unwrap_err().code, ExecutionErrorCode::SpawnError);
}

#[tokio::test]
async fn returns_an_aborted_result_for_pre_aborted_and_aborted_commands() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let error = env.exec("touch spawned".into(), Default::default(), &aborted_context()).await.unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::Aborted);
    assert!(!env.exists("spawned", &context()).await.unwrap());

    let (ctx, controller) = abortable();
    let running = env.exec("sleep 5".into(), Default::default(), &ctx);
    controller.abort(None);
    assert_eq!(running.await.unwrap_err().code, ExecutionErrorCode::Aborted);
}

// "ignores asynchronous taskkill spawn errors during abort" fakes `process.platform` as win32 on Unix to make Node's
// taskkill spawn fail; Rust picks the kill path at compile time, so the case does not apply.
