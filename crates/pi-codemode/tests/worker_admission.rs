//! Worker admission: the sandbox hook and the process-wide codemode limit, where scripts at the
//! limit wait in arrival order (upstream `codemode-worker-admission.test.ts` and
//! `worker-lifetime.test.ts`). Its own binary, since the limit is shared by every script in the
//! process.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_codemode::{
    AcquireWorker, CodemodeErrorKind, CodemodeHost, CodemodeSandbox, CodemodeToolInfo, CodemodeToolOptions,
    ExecuteOptions, MAX_CONCURRENT_CODEMODE_WORKERS, SandboxOptions, Timeout, execute_codemode,
    set_codemode_worker_limit,
};
use serde_json::json;
use support::*;
use tokio::sync::Semaphore;
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

/// Tests that change the process-wide limit run one at a time.
static LIMIT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A host whose `gate` tool blocks until `release_gate`, which keeps the script's worker alive,
/// and whose `mark` tool records the order in which scripts reach it.
struct Gated {
    host: Arc<dyn CodemodeHost>,
    gate: Arc<Semaphore>,
    entered: Arc<AtomicUsize>,
    marks: Arc<Mutex<Vec<i64>>>,
}

impl Gated {
    fn new() -> Self {
        let gate = Arc::new(Semaphore::new(0));
        let entered = Arc::new(AtomicUsize::new(0));
        let marks = Arc::new(Mutex::new(Vec::new()));
        let (held, counter, order) = (gate.clone(), entered.clone(), marks.clone());
        let schema = json!({ "type": "object", "properties": { "n": { "type": "number" } } });
        let host = FakeHost::default()
            .async_tool(CodemodeToolInfo::new("gate", "Block until released", schema.clone()), move |_| {
                let (held, counter) = (held.clone(), counter.clone());
                Box::pin(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    held.acquire().await.unwrap().forget();
                    text_outcome("released")
                })
            })
            .tool(CodemodeToolInfo::new("mark", "Record n", schema), move |args| {
                order.lock().unwrap().push(args["n"].as_i64().unwrap());
                text_outcome("marked")
            });
        Self { host: Arc::new(host), gate, entered, marks }
    }

    fn run(&self, id: &str, code: &str, cancel: CancellationToken) -> tokio::task::JoinHandle<String> {
        let (host, id, code) = (self.host.clone(), id.to_string(), code.to_string());
        tokio::spawn(async move {
            let options = CodemodeToolOptions::default();
            result_text(&execute_codemode(host, &id, &code, cancel, None, &options).await.unwrap())
        })
    }

    fn release_gate(&self, count: usize) {
        self.gate.add_permits(count);
    }
}

#[tokio::test]
async fn scripts_wait_for_a_free_worker_instead_of_failing() {
    assert_eq!(MAX_CONCURRENT_CODEMODE_WORKERS, 2);
    let _serial = LIMIT_LOCK.lock().await;
    let gated = Gated::new();
    set_codemode_worker_limit(2);
    let holders: Vec<_> = (0..2)
        .map(|index| gated.run(&format!("hold-{index}"), "await tools.gate({}); return 1", CancellationToken::new()))
        .collect();
    wait_for(|| gated.entered.load(Ordering::SeqCst) == 2).await;

    let waiting = gated.run("waiting", "return 3", CancellationToken::new());
    let abort = CancellationToken::new();
    let aborted = gated.run("aborted", "while (true) {}", abort.clone());
    tokio::time::sleep(Duration::from_millis(50)).await;
    abort.cancel();
    let text = aborted.await.unwrap();
    assert!(
        text.starts_with("Script error:\nScript aborted:") && text.ends_with("The script did not start."),
        "{text}"
    );

    let timed_out = gated.run("timed-out", "// @options: {\"timeout_ms\": 50}\nreturn 1", CancellationToken::new());
    let text = timed_out.await.unwrap();
    assert!(text.contains("Execution timed out after 50 ms while waiting for a free codemode worker"), "{text}");
    assert!(text.ends_with("The script did not start."), "{text}");
    assert!(!waiting.is_finished());

    // Releasing one actual worker admits exactly the waiting script.
    gated.release_gate(1);
    assert_eq!(waiting.await.unwrap(), "3");
    gated.release_gate(1);
    for holder in holders {
        assert_eq!(holder.await.unwrap(), "1");
    }
    set_codemode_worker_limit(usize::MAX);
}

#[tokio::test]
async fn waiting_scripts_start_in_arrival_order() {
    let _serial = LIMIT_LOCK.lock().await;
    let gated = Gated::new();
    set_codemode_worker_limit(1);
    let holder = gated.run("holder", "await tools.gate({}); return 0", CancellationToken::new());
    wait_for(|| gated.entered.load(Ordering::SeqCst) == 1).await;
    let mut queued = Vec::new();
    for n in 1..=4 {
        let code = format!("await tools.mark({{ n: {n} }}); return {n}");
        queued.push(gated.run(&format!("queued-{n}"), &code, CancellationToken::new()));
        // Each script joins the queue before the next one arrives.
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(gated.marks.lock().unwrap().is_empty());

    gated.release_gate(1);
    assert_eq!(holder.await.unwrap(), "0");
    for (index, handle) in queued.into_iter().enumerate() {
        assert_eq!(handle.await.unwrap(), (index + 1).to_string());
    }
    assert_eq!(*gated.marks.lock().unwrap(), [1, 2, 3, 4]);
    set_codemode_worker_limit(usize::MAX);
}

#[tokio::test]
async fn raising_the_limit_starts_waiting_scripts() {
    let _serial = LIMIT_LOCK.lock().await;
    let gated = Gated::new();
    set_codemode_worker_limit(1);
    let holder = gated.run("holder", "await tools.gate({}); return 0", CancellationToken::new());
    wait_for(|| gated.entered.load(Ordering::SeqCst) == 1).await;
    let waiting = gated.run("waiting", "return 5", CancellationToken::new());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished());

    set_codemode_worker_limit(2);
    assert_eq!(waiting.await.unwrap(), "5");
    gated.release_gate(1);
    assert_eq!(holder.await.unwrap(), "0");
    set_codemode_worker_limit(usize::MAX);
}

#[tokio::test]
async fn a_cancelled_script_gives_its_place_back() {
    let _serial = LIMIT_LOCK.lock().await;
    let gated = Gated::new();
    set_codemode_worker_limit(1);
    let holder = gated.run("holder", "await tools.gate({}); return 0", CancellationToken::new());
    wait_for(|| gated.entered.load(Ordering::SeqCst) == 1).await;
    let abort = CancellationToken::new();
    let leaving = gated.run("leaving", "return 1", abort.clone());
    let staying = gated.run("staying", "return 2", CancellationToken::new());
    tokio::time::sleep(Duration::from_millis(50)).await;
    abort.cancel();
    assert!(leaving.await.unwrap().contains("Script aborted"));

    gated.release_gate(1);
    assert_eq!(holder.await.unwrap(), "0");
    assert_eq!(staying.await.unwrap(), "2");
    set_codemode_worker_limit(usize::MAX);
}
