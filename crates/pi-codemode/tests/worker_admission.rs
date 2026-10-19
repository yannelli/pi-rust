//! Worker admission: the sandbox hook and the process-wide codemode limit (upstream
//! `codemode-worker-admission.test.ts` and `worker-lifetime.test.ts`). Its own binary, since the
//! limit is shared by every script in the process.

mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_codemode::{
    AcquireWorker, CodemodeErrorKind, CodemodeHost, CodemodeSandbox, CodemodeToolOptions, ExecuteOptions,
    MAX_CONCURRENT_CODEMODE_WORKERS, SandboxOptions, Timeout, execute_codemode, set_codemode_worker_limit,
};
use support::*;
use tokio_util::sync::CancellationToken;

async fn wait_for(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(condition(), "timed out");
}

#[tokio::test]
async fn admission_can_deny_a_worker_and_releases_when_the_thread_exits() {
    let denied = CodemodeSandbox::new(SandboxOptions {
        acquire_worker: Some(AcquireWorker(Arc::new(|| Err("no capacity".to_string())))),
        ..SandboxOptions::default()
    })
    .unwrap();
    let result = denied.execute("return 1", ExecuteOptions::default()).await.unwrap();
    let error = result.error().unwrap();
    assert_eq!((error.kind, error.message.as_str()), (CodemodeErrorKind::Sandbox, "no capacity"));

    let released = Arc::new(Mutex::new(0));
    let counter = released.clone();
    let admitted = CodemodeSandbox::new(SandboxOptions {
        acquire_worker: Some(AcquireWorker(Arc::new(move || {
            let counter = counter.clone();
            Ok(Box::new(move || *counter.lock().unwrap() += 1))
        }))),
        ..SandboxOptions::default()
    })
    .unwrap();
    let options =
        ExecuteOptions { timeout: Some(Timeout::After(Duration::from_millis(50))), ..ExecuteOptions::default() };
    let result = admitted.execute("while (true) {}", options).await.unwrap();
    assert_eq!(result.error().unwrap().kind, CodemodeErrorKind::Timeout);
    wait_for(|| *released.lock().unwrap() == 1).await;
}

#[tokio::test]
async fn codemode_calls_share_the_worker_limit_until_workers_exit() {
    assert_eq!(MAX_CONCURRENT_CODEMODE_WORKERS, 2);
    let host = Arc::new(FakeHost::standard());
    set_codemode_worker_limit(1);
    let dyn_host: Arc<dyn CodemodeHost> = host.clone();
    let cancel = CancellationToken::new();
    let options = CodemodeToolOptions::default();
    let running = {
        let (host, cancel, options) = (dyn_host.clone(), cancel.clone(), options.clone());
        tokio::spawn(async move { execute_codemode(host, "a", "while (true) {}", cancel, None, &options).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let blocked = execute_codemode(dyn_host.clone(), "b", "return 1", CancellationToken::new(), None, &options);
    let text = result_text(&blocked.await.unwrap());
    assert!(text.contains("Codemode worker limit reached (1 workers"), "{text}");

    cancel.cancel();
    running.await.unwrap().unwrap();
    let mut admitted = String::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let output = execute_codemode(dyn_host.clone(), "c", "return 2", CancellationToken::new(), None, &options);
        admitted = result_text(&output.await.unwrap());
        if !admitted.contains("worker limit") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(admitted, "2");
}
