//! Upstream `test/remote.test.ts`: transfer windows, cancellation, timeouts, lost connections, and large files.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::FutureExt;
use pi_chord::{AbortController, with_abort_signal};
use pi_durable::env::{FileSystem, Shell, ShellCommand, ShellExecOptions, ShellOutputWindow};
use pi_env::{
    Connection, ConnectionOptions, Json, ListFilesOptions, NodePlatform, RemoteExecutionEnv, RemoteExecutionEnvOptions,
    RequestOptions, node_platform,
};
use support::{ctx, environment};

fn argv(parts: &[&str]) -> ShellCommand {
    ShellCommand::Argv(parts.iter().map(|part| part.to_string()).collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transfers_only_about_a_window_of_output_a_caller_keeps_with_exact_counts() {
    let (env, _guard) = environment();
    let total = 5_000_000usize;
    let seen = Arc::new(Mutex::new((0usize, 0usize, String::new())));
    let sink = seen.clone();
    let options = ShellExecOptions {
        window: Some(ShellOutputWindow {
            max_bytes: 1000,
            max_lines: 20,
            min_interval_ms: 20.0,
            bytes_per_second: 1_000_000.0,
        }),
        on_output: Some(Arc::new(move |text, _, info| {
            let mut seen = sink.lock().unwrap();
            seen.0 += text.len();
            if let Some(skipped) = info.skipped {
                seen.1 += skipped.bytes as usize;
                seen.2.clear();
            }
            seen.2.push_str(text);
        })),
        ..Default::default()
    };
    let command = format!("yes 0123456789abcdef | head -c {total}");
    let result = env.exec(argv(&["sh", "-c", &command]), options, &ctx()).await.expect("exec");
    assert_eq!(result.exit_code, 0);
    let (delivered, skipped, tail) = seen.lock().unwrap().clone();
    assert_eq!(delivered + skipped, total);
    assert!(skipped > 0);
    assert!(delivered < total / 10);
    // After the last skip comes everything to the end, more than the window.
    let full: String = "0123456789abcdef\n".repeat(total.div_ceil(17))[..total].to_string();
    assert!(full.ends_with(&tail));
    assert!(tail.len() > 1000 || tail.matches('\n').count() > 20);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborts_commands_cancelled_right_after_they_are_sent() {
    let (env, _guard) = environment();
    let started = Instant::now();
    for attempt in 0..20u64 {
        let controller = AbortController::new();
        let context = with_abort_signal(&controller.signal(), &ctx());
        let runner = env.clone();
        let running =
            tokio::spawn(
                async move { runner.exec(argv(&["sleep", "5"]), ShellExecOptions::default(), &context).await },
            );
        if attempt % 2 == 0 {
            controller.abort(None);
        } else {
            tokio::time::sleep(Duration::from_millis(attempt % 5)).await;
            controller.abort(None);
        }
        let result = running.await.expect("join");
        assert_eq!(result.map(|_| "ok".to_string()).unwrap_or_else(|error| error.code.as_str().into()), "aborted");
    }
    assert!(started.elapsed() < Duration::from_secs(10));
}

fn names(platform: &str, arch: &str) -> NodePlatform {
    NodePlatform { platform: platform.into(), arch: arch.into() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reports_the_remote_system_in_nodes_names() {
    let (_env, guard) = environment();
    let info = guard.connection.info().await.expect("info");
    let platform = match () {
        _ if cfg!(windows) => "win32",
        _ if cfg!(target_os = "macos") => "darwin",
        _ if cfg!(target_os = "android") => "android",
        _ => "linux",
    };
    let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" };
    assert_eq!(node_platform(&info.os, &info.arch), names(platform, arch));
    assert_eq!(info.node_platform(), names(platform, arch));
    assert_eq!(node_platform("windows", "aarch64"), names("win32", "arm64"));
    assert_eq!(node_platform("macos", "x86_64"), names("darwin", "x64"));
    assert_eq!(node_platform("freebsd", "riscv64"), names("freebsd", "riscv64"));
}

/// A stand-in for a daemon released before `hello.features`: it answers `hello` without `features` and nothing else.
#[cfg(unix)]
fn old_daemon(dir: &std::path::Path) -> Connection {
    let hello = serde_json::json!({
        "protocol": 1, "version": "1.1.0", "os": "linux", "arch": "x86_64", "home": "/", "separator": "/",
        "tmpdir": "/tmp", "cwd": "/", "driveCwds": {}, "pid": 1,
    })
    .to_string();
    let mut frame = Vec::new();
    frame.extend(((9 + hello.len()) as u32).to_be_bytes());
    frame.push(2);
    frame.extend(1u32.to_be_bytes());
    frame.extend((hello.len() as u32).to_be_bytes());
    frame.extend(hello.as_bytes());
    let file = dir.join("hello.frame");
    std::fs::write(&file, frame).expect("write the frame");
    // Called as `serve --token <hex>`: the token is `$3`. The reply waits for the client's first request.
    let script = r#"printf 'PI-ENV %s\n' "$3"; head -c 1 >/dev/null; cat "$0"; cat >/dev/null"#;
    let command = vec!["sh".to_string(), "-c".into(), script.into(), file.to_string_lossy().into_owned()];
    Connection::new(ConnectionOptions::new(command))
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reports_list_files_as_not_supported_by_daemons_released_before_it() {
    let (env, guard) = environment();
    let features = guard.connection.info().await.expect("info").features.clone();
    assert!(features.is_some_and(|features| features.contains(&"listFiles".to_string())));
    let connection = old_daemon(guard.dir.path());
    let old = RemoteExecutionEnv::new(RemoteExecutionEnvOptions::new(connection.clone(), "pi-env:old", env.cwd()));
    let listed = tokio::time::timeout(Duration::from_secs(20), old.list_files(".", None, &ctx())).await;
    connection.close();
    let error = listed.expect("the old daemon answers hello").expect_err("not supported");
    assert_eq!(error.code.as_str(), "not_supported");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancels_a_listing_in_the_daemon() {
    let (env, guard) = environment();
    for dir in 0..100 {
        let path = guard.dir.path().join(format!("d{dir}"));
        std::fs::create_dir(&path).unwrap();
        for file in 0..100 {
            std::fs::write(path.join(format!("f{file}")), "").unwrap();
        }
    }
    // The cancel follows the request at once, bypassing the client's own abort check, so the daemon stops the walk.
    let controller = AbortController::new();
    controller.abort(None);
    let mut json = Json::new();
    json.insert("path".into(), env.cwd().into());
    let options = RequestOptions { signal: Some(controller.signal()), ..Default::default() };
    let cancelled = guard.connection.request("listFiles", json, options).await;
    assert_eq!(cancelled.expect_err("aborted").code, "aborted");
    let listed = env.list_files(".", None, &ctx()).await.expect("list");
    assert_eq!((listed.entries.len(), listed.truncated), (10_100, false));
    let bounded = ListFilesOptions { max_entries: Some(5000), ..Default::default() };
    let bounded = env.list_files(".", Some(&bounded), &ctx()).await.expect("list");
    assert_eq!((bounded.entries.len(), bounded.truncated), (5000, true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn times_out_a_command_that_produces_no_output() {
    let (env, _guard) = environment();
    let started = Instant::now();
    let options = ShellExecOptions { timeout: Some(0.3), ..Default::default() };
    let result = env.exec(argv(&["sleep", "5"]), options, &ctx()).await;
    assert_eq!(result.map(|_| "ok".to_string()).unwrap_or_else(|error| error.code.as_str().into()), "timeout");
    assert!(started.elapsed() < Duration::from_secs(3));
}

/// Killing the daemon by pid needs `kill`, so this case runs on Unix only.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fails_requests_on_handles_of_a_lost_connection_instead_of_reaching_the_new_daemon() {
    let (env, guard) = environment();
    support::write(env.as_ref(), "a.txt", b"AAA").await;
    support::write(env.as_ref(), "b.txt", b"BBB").await;
    let a = env.open_binary_reader("a.txt", false, &ctx()).await.expect("open a");
    let pid = guard.connection.info().await.expect("info").pid;
    std::process::Command::new("kill").args(["-9", &pid.to_string()]).status().expect("kill");
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The new daemon numbers its handles from 1 again.
    let b = env.open_binary_reader("b.txt", false, &ctx()).await.expect("open b");
    assert_ne!(guard.connection.info().await.expect("info").pid, pid);
    let stale = a.read(0, 3, &ctx()).await;
    assert_eq!(
        stale.map(|bytes| String::from_utf8(bytes).unwrap()).unwrap_or_else(|e| e.code.as_str().into()),
        "unknown"
    );
    assert_eq!(b.read(0, 3, &ctx()).await.expect("read b"), b"BBB");
    a.close(&ctx()).await;
    b.close(&ctx()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn computes_the_command_at_each_start_and_retries_a_start_that_failed() {
    let dir = tempfile::Builder::new().prefix("pi-env-remote-").tempdir().unwrap();
    let starts = Arc::new(AtomicUsize::new(0));
    let counter = starts.clone();
    let daemon = support::daemon();
    let connection = Connection::new(ConnectionOptions::computed(Arc::new(move || {
        let (counter, daemon) = (counter.clone(), daemon.clone());
        async move {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err("ssh gpu-box failed: Network is unreachable".to_string());
            }
            Ok(vec![daemon])
        }
        .boxed()
    })));
    let cwd = dir.path().to_string_lossy().into_owned();
    let env = RemoteExecutionEnv::new(RemoteExecutionEnvOptions::new(connection.clone(), "pi-env:test", cwd));
    let offline = env.read_text_file("missing.txt", &ctx()).await.expect_err("offline");
    assert_eq!(
        (offline.code.as_str(), offline.message.as_str()),
        ("unknown", "ssh gpu-box failed: Network is unreachable")
    );
    let command = env.exec(argv(&["sh", "-c", "exit 0"]), ShellExecOptions::default(), &ctx()).await;
    assert_eq!(command.expect("exec").exit_code, 0);
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_and_reads_files_larger_than_one_transfer_chunk_in_order() {
    let (env, _guard) = environment();
    let content: Vec<u8> = (0..3_500_017u64).map(|index| ((index * 7919) % 251) as u8).collect();
    env.write_file("big.bin", &content, &ctx()).await.expect("write");
    env.append_file("big.bin", &content, &ctx()).await.expect("append");
    let read = env.read_binary_file("big.bin", &ctx()).await.expect("read");
    assert_eq!(read.len(), content.len() * 2);
    assert!(read[..content.len()] == content[..]);
    assert!(read[content.len()..] == content[..]);
    let reader = env.open_binary_reader("big.bin", false, &ctx()).await.expect("open");
    let range = reader.read(1_000_003, 2_000_000, &ctx()).await.expect("range");
    reader.close(&ctx()).await;
    assert!(range[..] == read[1_000_003..3_000_003]);
}
