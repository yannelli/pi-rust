//! Ported from upstream `packages/codemode/test/deterministic.test.ts`: `start()` runs a script
//! deterministically with the caller answering its calls, so a durable host can replay it.

mod common;

use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use common::sandbox;
use pi_codemode::{
    CodemodeCallStatus, CodemodeCallTarget, CodemodeDeclaredGlobal, CodemodeDeclaredTool, CodemodeDuring,
    CodemodeErrorKind, CodemodeHostCall, CodemodeOutcome, CodemodeOutputItem, CodemodeResult, CodemodeRun,
    CodemodeRunOptions, CodemodeSandbox, ExecuteOptions, RunDeliverer, SandboxOptions, Timeout,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// 2026-01-15T10:00:00Z in ms since the epoch.
const STARTED_AT: f64 = 1_768_471_200_000.0;
/// The first two `Math.random()` values with seed 0.
const PINNED_SEED_ZERO: [f64; 2] = [0.6711372530266764, 0.7258461452833667];
/// The first `Math.random()` with seed 1.
const PINNED_FIRST_RANDOM: f64 = 0.34227450605335275;

type Answer = Arc<dyn Fn(&CodemodeHostCall) -> CodemodeOutcome + Send + Sync>;

struct Recorded {
    run: CodemodeRun,
    calls: Arc<Mutex<Vec<CodemodeHostCall>>>,
    idles: Arc<Mutex<Vec<(u64, u64)>>>,
    events: Arc<Mutex<Vec<String>>>,
    fingerprint: Arc<Mutex<Option<String>>>,
}

impl Recorded {
    fn calls(&self) -> Vec<CodemodeHostCall> {
        self.calls.lock().unwrap().clone()
    }

    fn idles(&self) -> Vec<(u64, u64)> {
        self.idles.lock().unwrap().clone()
    }

    async fn wait_for(&self, check: impl Fn(&Recorded) -> bool) {
        for _ in 0..1000 {
            if check(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("Condition was not reached");
    }
}

fn echo_answer() -> Answer {
    Arc::new(|call| CodemodeOutcome::Ok(call.args.clone()))
}

fn ok() -> CodemodeOutcome {
    CodemodeOutcome::Ok(Some(Value::Null))
}

/// Start `code` with tools `echo` and `wait`. Every call but `wait` is answered at once, by default
/// with its argument, as result number 1, 2, ... with the clock at `STARTED_AT + 1000 * seq`; `wait` is
/// left for the test to answer.
fn start_with(
    sandbox: &CodemodeSandbox,
    code: &str,
    answer: Answer,
    configure: impl FnOnce(&mut CodemodeRunOptions),
) -> Recorded {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let idles = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let fingerprint = Arc::new(Mutex::new(None));
    let deliverer = Arc::new(OnceLock::<RunDeliverer>::new());
    let next_seq = Arc::new(Mutex::new(0_u64));
    let mut options = CodemodeRunOptions::new(STARTED_AT, 1, {
        let (calls, events, deliverer) = (calls.clone(), events.clone(), deliverer.clone());
        move |call: CodemodeHostCall| {
            calls.lock().unwrap().push(call.clone());
            events.lock().unwrap().push(format!("call {}", call.id));
            if call.name == "wait" {
                return;
            }
            let mut seq = next_seq.lock().unwrap();
            *seq += 1;
            let now = STARTED_AT + 1000.0 * *seq as f64;
            deliverer.get().expect("the run exists").deliver(*seq, now, call.id, answer(&call)).unwrap();
        }
    });
    options.tools = vec![CodemodeDeclaredTool::new("echo"), CodemodeDeclaredTool::new("wait")];
    options.timeout = Timeout::After(Duration::from_secs(20));
    options.on_started = Some(Arc::new({
        let (fingerprint, events) = (fingerprint.clone(), events.clone());
        move |value| {
            *fingerprint.lock().unwrap() = Some(value.to_string());
            events.lock().unwrap().push("started".into());
        }
    }));
    options.on_idle = Some(Arc::new({
        let (idles, events) = (idles.clone(), events.clone());
        move |seq, issued| {
            idles.lock().unwrap().push((seq, issued));
            events.lock().unwrap().push(format!("idle {seq}"));
        }
    }));
    configure(&mut options);
    let run = sandbox.start(code, options).expect("run starts");
    deliverer.set(run.deliverer()).ok();
    Recorded { run, calls, idles, events, fingerprint }
}

fn start(sandbox: &CodemodeSandbox, code: &str) -> Recorded {
    start_with(sandbox, code, echo_answer(), |_| {})
}

fn value(result: &CodemodeResult) -> Value {
    result.value().cloned().unwrap_or(Value::Null)
}

type Timeless = (Result<Option<Value>, String>, Vec<CodemodeOutputItem>, Vec<(String, CodemodeCallStatus)>);

/// Everything but the wall-clock call durations.
fn timeless(result: &CodemodeResult) -> Timeless {
    let outcome = result.outcome.clone().map(|success| success.value).map_err(|error| error.message);
    (outcome, result.output.clone(), result.calls.iter().map(|call| (call.name.clone(), call.status)).collect())
}

#[tokio::test]
async fn makes_the_same_calls_and_output_for_the_same_start_seed_and_results() {
    let code = r#"
        const values = [Date.now(), performance.now(), Math.random(), Math.random(), new Date().toISOString()];
        const first = await tools.echo(values);
        text(JSON.stringify([Date.now(), performance.now(), Math.random()]));
        return await tools.echo(first.concat(Math.random()));
    "#;
    let sandbox = sandbox(vec![]);
    let one = start(&sandbox, code);
    let two = start(&sandbox, code);
    let (first_calls, second_calls) = (one.calls.clone(), two.calls.clone());
    let a = one.run.result.await;
    let b = two.run.result.await;
    assert!(a.is_ok(), "{a:?}");
    assert_eq!(timeless(&a), timeless(&b));
    assert_eq!(*first_calls.lock().unwrap(), *second_calls.lock().unwrap());
    // The clock: started_at, then the time of the result being processed; performance.now() counts from it.
    let args = first_calls.lock().unwrap()[0].args.clone().unwrap();
    assert_eq!(args[0], json!(STARTED_AT as i64));
    assert_eq!(args[1], json!(0));
    assert_eq!(args[4], json!("2026-01-15T10:00:00.000Z"));
    let CodemodeOutputItem::Text { text, .. } = &a.output[0] else { panic!("text output") };
    let printed: Vec<f64> = serde_json::from_str(text).unwrap();
    assert_eq!(printed[..2], [STARTED_AT + 1000.0, 1000.0]);
    // Pinned: a change here changes what replayed scripts see, so it must change the fingerprint too.
    let random = args[2].as_f64().unwrap();
    assert_eq!(random, PINNED_FIRST_RANDOM);

    // The seed decides the random values; the start time does not.
    let later = start_with(&sandbox, code, echo_answer(), |options| options.started_at = STARTED_AT + 5000.0);
    let reseeded = start_with(&sandbox, code, echo_answer(), |options| options.seed = 2);
    let (later_calls, reseeded_calls) = (later.calls.clone(), reseeded.calls.clone());
    later.run.result.await;
    reseeded.run.result.await;
    assert_eq!(later_calls.lock().unwrap()[0].args.as_ref().unwrap()[2].as_f64(), Some(random));
    assert_ne!(reseeded_calls.lock().unwrap()[0].args.as_ref().unwrap()[2].as_f64(), Some(random));
    // Seed 0 has its own values: QuickJS turns a zero state into 1, which must not make it seed 1's.
    let zero = start_with(&sandbox, "return [Math.random(), Math.random()]", echo_answer(), |o| o.seed = 0);
    assert_eq!(value(&zero.run.result.await), json!(PINNED_SEED_ZERO));
}

/// Runs the test again in a child process with the host timezone set: the engine reads it at startup.
fn in_zone(test: &str, zone: &str) -> bool {
    if std::env::var_os("PI_CODEMODE_ZONE_CHILD").is_some() {
        return true;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact"])
        .env("TZ", zone)
        .env("PI_CODEMODE_ZONE_CHILD", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success() && stdout.contains("1 passed"), "{stdout}{stderr}");
    false
}

#[tokio::test]
async fn runs_in_utc_whatever_the_hosts_timezone_and_without_weakref() {
    if !in_zone("runs_in_utc_whatever_the_hosts_timezone_and_without_weakref", "IST-5:30") {
        return;
    }
    let code =
        "return [new Date(0).getHours(), new Date(0).getTimezoneOffset(), typeof WeakRef, typeof FinalizationRegistry]";
    let sandbox = sandbox(vec![]);
    let result = start(&sandbox, code).run.result.await;
    assert_eq!(value(&result), json!([0, 0, "undefined", "undefined"]));
    // execute() is unchanged: the host's timezone and every intrinsic.
    let result = sandbox.execute(code, ExecuteOptions::default()).await.unwrap();
    assert_eq!(value(&result), json!([5, -330, "function", "function"]));
}

#[tokio::test]
async fn reads_and_writes_local_time_as_utc() {
    if !in_zone("reads_and_writes_local_time_as_utc", "PST8PDT") {
        return;
    }
    let code = r#"
        const date = new Date(2026, 0, 15, 10, 30, 5, 7);
        const written = new Date(0);
        written.setHours(23, 59);
        return [
            date.toISOString(), date.getDate(), date.getDay(), date.getMonth(), date.getFullYear(), date.getMilliseconds(),
            date.toString(), date.toDateString(), date.toTimeString(),
            date.toLocaleString(), date.toLocaleDateString(), date.toLocaleTimeString(),
            written.toISOString(), String(new Date(NaN)), Date().length > 0,
            new Date("2026-01-15T10:00:00").toISOString(), new Date("2026-01-15T10:00:00+02:00").toISOString(),
            new Date("2026-01-15").toISOString(), new Date("Jan 15 2026 10:00").toISOString(),
            new Date("Thu, 15 Jan 2026 10:00:00 GMT").toISOString(), Date.parse("2026-01-15T10:00:00Z"),
            new Date(2026, 0).getTime() === Date.UTC(2026, 0),
        ];
    "#;
    let result = start(&sandbox(vec![]), code).run.result.await;
    let expected = json!([
        "2026-01-15T10:30:05.007Z",
        15,
        4,
        0,
        2026,
        7,
        "Thu Jan 15 2026 10:30:05 GMT+0000",
        "Thu Jan 15 2026",
        "10:30:05 GMT+0000",
        "01/15/2026, 10:30:05 AM",
        "01/15/2026",
        "10:30:05 AM",
        "1970-01-01T23:59:00.000Z",
        "Invalid Date",
        true,
        "2026-01-15T10:00:00.000Z",
        "2026-01-15T08:00:00.000Z",
        "2026-01-15T00:00:00.000Z",
        "2026-01-15T10:00:00.000Z",
        "2026-01-15T10:00:00.000Z",
        1_768_471_200_000_i64,
        true,
    ]);
    assert_eq!(value(&result), expected, "{result:?}");
}

#[tokio::test]
async fn applies_results_in_seq_order_whatever_order_they_are_delivered_in() {
    let sandbox = sandbox(vec![]);
    let code = r#"const order = [];
        await Promise.all([tools.wait("a").then(() => order.push("a")), tools.wait("b").then(() => order.push("b"))]);
        return order;"#;
    let recorded = start(&sandbox, code);
    recorded.wait_for(|r| r.calls().len() == 2 && r.idles().len() == 1).await;
    let calls = recorded.calls();
    // b is result 1 and a result 2, but a arrives first.
    recorded.run.deliver(2, STARTED_AT + 2000.0, calls[0].id, ok()).unwrap();
    recorded.run.deliver(1, STARTED_AT + 1000.0, calls[1].id, ok()).unwrap();
    let idles = recorded.idles.clone();
    let result = recorded.run.result.await;
    assert_eq!(value(&result), json!(["b", "a"]));
    // No idle after the last result: the script finished in that drain, and the host stopped the VM.
    assert_eq!(*idles.lock().unwrap(), [(0, 2), (1, 2)]);
}

fn sandbox_with_heap(limit: usize) -> CodemodeSandbox {
    CodemodeSandbox::new(SandboxOptions { memory_limit_bytes: Some(limit), ..SandboxOptions::default() }).unwrap()
}

#[tokio::test]
async fn reports_the_fingerprint_before_any_call_and_the_calls_issued_after_each_result() {
    let sandbox = sandbox_with_heap(64 * 1024 * 1024);
    let recorded = start(&sandbox, "await tools.echo(1); await Promise.all([tools.echo(2), tools.echo(3)]);");
    let (events, idles, calls) = (recorded.events.clone(), recorded.idles.clone(), recorded.calls.clone());
    let fingerprint = recorded.fingerprint.clone();
    assert!(recorded.run.result.await.is_ok());
    assert_eq!(events.lock().unwrap()[..3], ["started", "call 1", "idle 0"]);
    assert_eq!(*idles.lock().unwrap(), [(0, 1), (1, 3), (2, 3)]);
    let summary: Vec<_> =
        calls.lock().unwrap().iter().map(|call| (call.id, call.target, call.name.clone(), call.args.clone())).collect();
    let call = |id, name: &str, arg| (id, CodemodeCallTarget::Tool, name.to_string(), Some(json!(arg)));
    assert_eq!(summary, [call(1, "echo", 1), call(2, "echo", 2), call(3, "echo", 3)]);
    let fingerprint = fingerprint.lock().unwrap().clone().unwrap();
    let words: Vec<&str> = fingerprint.split(", ").collect();
    assert!(words[0].starts_with("rquickjs "), "{fingerprint}");
    assert!(words[1].starts_with("prelude ") && words[1].len() == "prelude ".len() + 8, "{fingerprint}");
    assert!(words[2].starts_with("deterministic ") && words[3].starts_with("stack "), "{fingerprint}");
    assert_eq!((words[4], words[5]), ("heap 67108864", "budget none"));

    let same = start(&sandbox, "return 1");
    let other = start(&sandbox_with_heap(32 * 1024 * 1024), "return 1");
    let (same_fingerprint, other_fingerprint) = (same.fingerprint.clone(), other.fingerprint.clone());
    same.run.result.await;
    other.run.result.await;
    assert_eq!(same_fingerprint.lock().unwrap().as_deref(), Some(fingerprint.as_str()));
    assert_ne!(other_fingerprint.lock().unwrap().as_deref(), Some(fingerprint.as_str()));
}

#[tokio::test]
async fn numbers_calls_to_globals_with_tools_and_answers_errors_as_rejections() {
    let code = r#"const found = await search("x", 2);
        try { await tools.echo("fail"); } catch (error) { text(error.message); }
        return found;"#;
    let answer: Answer = Arc::new(|call| {
        if call.args == Some(json!("fail")) {
            CodemodeOutcome::Err { message: "no".into(), properties: None }
        } else if call.name == "search" {
            CodemodeOutcome::Ok(Some(json!(["found"])))
        } else {
            CodemodeOutcome::Ok(call.args.clone())
        }
    });
    let recorded = start_with(&sandbox(vec![]), code, answer, |options| {
        options.globals = vec![CodemodeDeclaredGlobal { name: "search".into(), spread: true }];
    });
    let (calls, idles) = (recorded.calls.clone(), recorded.idles.clone());
    let result = recorded.run.result.await;
    let calls = calls.lock().unwrap().clone();
    let search = CodemodeHostCall {
        id: 1,
        target: CodemodeCallTarget::Global,
        name: "search".into(),
        args: Some(json!(["x", 2])),
    };
    assert_eq!(calls[0], search);
    assert_eq!(value(&result), json!(["found"]));
    assert_eq!(result.output, [CodemodeOutputItem::Text { text: "no".into(), console: false }]);
    assert_eq!((calls[1].id, calls[1].target, calls[1].name.as_str()), (2, CodemodeCallTarget::Tool, "echo"));
    // Calls to globals count as issued calls too.
    assert_eq!(idles.lock().unwrap()[..2], [(0, 1), (1, 2)]);
    let recorded: Vec<_> = result.calls.iter().map(|call| (call.name.as_str(), call.status)).collect();
    assert_eq!(recorded, [("echo", CodemodeCallStatus::Error)]);
}

#[tokio::test]
async fn keeps_the_clock_monotonic_and_rejects_a_clock_value_that_is_not_finite() {
    let sandbox = sandbox(vec![]);
    let code = "const before = Date.now(); await tools.wait(1); return [before, Date.now(), performance.now()]";
    let recorded = start(&sandbox, code);
    recorded.wait_for(|r| r.calls().len() == 1).await;
    let error = recorded.run.deliver(1, f64::NAN, 1, ok()).unwrap_err();
    assert!(error.0.contains("not a finite number"), "{error}");
    // A resumed host whose clock is behind the run's start.
    recorded.run.deliver(1, STARTED_AT - 2000.0, 1, ok()).unwrap();
    let result = recorded.run.result.await;
    let started = STARTED_AT as i64;
    assert_eq!(value(&result), json!([started, started, 0]));
}

#[tokio::test]
async fn reports_the_result_the_vm_was_processing_when_the_script_ended() {
    let sandbox = sandbox(vec![]);
    let returned = start(&sandbox, "await tools.echo(1); return await tools.echo(2)");
    let thrown =
        start(&sandbox, "await tools.echo(1); await Promise.all([tools.echo(2), tools.echo(3)]); throw new Error('x')");
    let first = start(&sandbox, "return 1");
    let returned = returned.run.result.await;
    assert_eq!((value(&returned), returned.during), (json!(2), Some(CodemodeDuring { seq: 2, issued: 2 })));
    let thrown = thrown.run.result.await;
    assert!(!thrown.is_ok());
    assert_eq!(thrown.during, Some(CodemodeDuring { seq: 3, issued: 3 }));
    assert_eq!(first.run.result.await.during, Some(CodemodeDuring { seq: 0, issued: 0 }));
    let executed = sandbox.execute("return 1", ExecuteOptions::default()).await.unwrap();
    assert_eq!(executed.during, None);
}

#[tokio::test]
async fn rejects_with_an_error_carrying_the_answers_properties() {
    let code = "try { await tools.echo(1); } catch (e) { return [e instanceof Error, e.message, e.name, e.structuredOutput]; }";
    let answer: Answer = Arc::new(|_| {
        let properties = json!({ "structuredOutput": { "code": 7 }, "name": "Ignored", "message": "ignored" });
        let Value::Object(properties) = properties else { unreachable!() };
        CodemodeOutcome::Err { message: "denied".into(), properties: Some(properties) }
    });
    let recorded = start_with(&sandbox(vec![]), code, answer, |_| {});
    let result = recorded.run.result.await;
    assert_eq!(value(&result), json!([true, "denied", "Error", { "code": 7 }]));
}

#[tokio::test]
async fn rejects_tools_sharing_a_script_name_and_uses_the_script_names_the_run_passes() {
    let sandbox = sandbox(vec![]);
    let started = |tools: Vec<CodemodeDeclaredTool>| {
        let mut options = CodemodeRunOptions::new(STARTED_AT, 1, |_| {});
        options.tools = tools;
        sandbox.start("", options)
    };
    let error = started(vec![CodemodeDeclaredTool::new("a-b"), CodemodeDeclaredTool::new("a_b")]).unwrap_err();
    assert!(error.0.contains("script names must be unique"), "{error}");
    let error = started(vec![CodemodeDeclaredTool::new("a"), CodemodeDeclaredTool::new("a")]).unwrap_err();
    assert_eq!(error.0, "Tool names must be unique");

    let code = "return [typeof tools.renamed, Object.keys(tools), ALL_TOOLS.map((tool) => tool.name)]";
    let recorded = start_with(&sandbox, code, echo_answer(), |options| {
        let tool = CodemodeDeclaredTool { name: "my-tool".into(), js_name: Some("renamed".into()), description: None };
        options.tools = vec![tool];
    });
    let result = recorded.run.result.await;
    assert_eq!(value(&result), json!(["function", ["renamed", "my-tool"], ["renamed"]]));
}

#[tokio::test]
async fn ends_with_an_error_the_script_cannot_catch_when_the_caller_cancels() {
    let cancel = CancellationToken::new();
    let code = r#"try { await tools.wait(1); } catch { text("caught"); }
        text("after");"#;
    let recorded = start_with(&sandbox(vec![]), code, echo_answer(), |options| options.cancel = Some(cancel.clone()));
    recorded.wait_for(|r| r.calls().len() == 1).await;
    cancel.cancel();
    let deliverer = recorded.run.deliverer();
    let result = recorded.run.result.await;
    assert_eq!(result.error().unwrap().kind, CodemodeErrorKind::Aborted);
    assert!(result.output.is_empty());
    assert!(deliverer.deliver(1, STARTED_AT, 1, ok()).is_ok());
}

#[tokio::test]
async fn is_finished_once_it_has_its_outcome() {
    let recorded = start(&sandbox(vec![]), "await tools.wait(1); return 2");
    recorded.wait_for(|r| r.calls().len() == 1).await;
    assert!(!recorded.run.finished());
    recorded.run.deliver(1, STARTED_AT, 1, ok()).unwrap();
    let deliverer = recorded.run.deliverer();
    recorded.run.result.await;
    assert!(deliverer.finished());
}

#[tokio::test]
async fn rejects_a_call_that_is_not_waiting_and_a_result_number_used_twice() {
    let recorded = start(&sandbox(vec![]), "await Promise.all([tools.wait(1), tools.wait(2)])");
    recorded.wait_for(|r| r.calls().len() == 2).await;
    let run = &recorded.run;
    assert_eq!(run.deliver(1, STARTED_AT, 99, ok()).unwrap_err().0, "Call 99 is not waiting for a result");
    run.deliver(1, STARTED_AT, 1, ok()).unwrap();
    assert!(run.deliver(1, STARTED_AT, 2, ok()).unwrap_err().0.contains("used once"));
    assert!(run.deliver(0, STARTED_AT, 2, ok()).unwrap_err().0.contains("used once"));
    run.deliver(2, STARTED_AT, 2, ok()).unwrap();
    assert!(recorded.run.result.await.is_ok());
}

#[tokio::test]
async fn rejects_an_invalid_start_and_a_closed_sandbox() {
    let sandbox = sandbox(vec![]);
    let options = |started_at| CodemodeRunOptions::new(started_at, 1, |_| {});
    assert!(sandbox.start("", options(-1.0)).unwrap_err().0.contains("non-negative"));
    assert!(sandbox.start("", options(f64::NAN)).is_err());
    let mut invalid = options(0.0);
    invalid.globals = vec![CodemodeDeclaredGlobal::new("tools")];
    assert_eq!(sandbox.start("", invalid).unwrap_err().0, "Invalid global name \"tools\"");
    sandbox.close();
    assert_eq!(sandbox.start("", options(0.0)).unwrap_err().0, "Sandbox is closed");
}

#[tokio::test]
async fn execute_reads_the_real_clock_and_real_randomness() {
    let sandbox = sandbox(vec![]);
    let epoch = std::time::UNIX_EPOCH;
    let before = std::time::SystemTime::now().duration_since(epoch).unwrap().as_millis() as f64;
    let first = sandbox.execute("return [Date.now(), Math.random()]", ExecuteOptions::default()).await.unwrap();
    let second = sandbox.execute("return [Date.now(), Math.random()]", ExecuteOptions::default()).await.unwrap();
    let first = value(&first);
    assert!(first[0].as_f64().unwrap() >= before);
    assert_ne!(first[1], value(&second)[1]);
}
