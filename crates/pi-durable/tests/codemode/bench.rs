//! Port of upstream `test/codemode-bench.ts`: durable code mode overhead. One model-issued call makes N calls of a
//! trivial `echo` tool, as a code mode script or as a plain tool calling `execute_tool()`. Run it with
//! `CODEMODE_BENCH_SIZES=100,1000 cargo test -p pi-durable --release --test codemode -- --ignored --nocapture bench`.

use std::time::Instant;

use super::*;

fn echo() -> ToolRegistration {
    let parameters = json!({ "type": "object", "properties": { "i": { "type": "number" } }, "required": ["i"] });
    define_tool("echo", "echo", parameters, |args, _, _| async move { Ok(ToolExecutionResult::text(arg(&args, "i"))) })
}

fn plain() -> ToolRegistration {
    let parameters = json!({
        "type": "object",
        "properties": { "n": { "type": "number" }, "shape": { "type": "string" } },
        "required": ["n", "shape"],
    });
    let tool = define_tool("plain", "plain", parameters, |args, api, context| async move {
        let n = args.get("n").and_then(Value::as_u64).unwrap_or(0);
        let call = |i: u64| {
            let args = json!({ "i": i }).as_object().cloned().unwrap_or_default();
            api.execute_tool("echo", args, &context, ExecuteToolOptions::default())
        };
        if args.get("shape") == Some(&json!("sequential")) {
            for i in 0..n {
                call(i).await?;
            }
        } else {
            for result in futures::future::join_all((0..n).map(call)).await {
                result?;
            }
        }
        Ok(ToolExecutionResult::text("done"))
    });
    tool.with_replay(ToolReplay::Safe)
}

/// Wall time in ms and commits of one run.
async fn run(mode: &str, shape: &str, n: u64) -> (f64, usize) {
    let directory = tempfile::tempdir().unwrap();
    let setup = chat_setup(Default::default());
    let options = SandboxOptions { memory_limit_bytes: Some(256 * 1024 * 1024), ..SandboxOptions::default() };
    let sandbox = Arc::new(CodemodeSandbox::new(options).unwrap());
    add_tool(&setup, echo());
    add_tool(&setup, plain());
    add_tool(&setup, create_codemode_tool(CodemodeToolOptions::new(sandbox)));
    let code = match shape {
        "sequential" => format!("for (let i = 0; i < {n}; i++) await tools.echo({{ i }}); return \"done\";"),
        _ => {
            format!("await Promise.all(Array.from({{ length: {n} }}, (_, i) => tools.echo({{ i }}))); return \"done\";")
        }
    };
    let call = match mode {
        "codemode" => ("codemode", json!({ "code": code }), "c1"),
        _ => ("plain", json!({ "n": n, "shape": shape }), "c1"),
    };
    setup.faux.set_responses([calls(&[call]), answer("ok")]);
    let (harness, root) = open(&sqlite_path(&directory), &setup).await;
    let commits = Arc::new(AtomicUsize::new(0));
    let counter = commits.clone();
    let listener = harness
        .subscribe_commits(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    let started = Instant::now();
    let id = submit(&root, "go").await;
    wait(&harness, id).await;
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    drop(listener);
    harness.close(&ctx()).await.unwrap();
    (ms, commits.load(Ordering::SeqCst))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark"]
async fn bench_codemode_overhead() {
    let sizes = std::env::var("CODEMODE_BENCH_SIZES").unwrap_or_else(|_| "100,1000".into());
    let sizes: Vec<u64> = sizes.split(',').filter_map(|size| size.trim().parse().ok()).collect();
    for shape in ["sequential", "parallel"] {
        for &n in &sizes {
            let (plain_ms, plain_commits) = run("plain", shape, n).await;
            let (codemode_ms, codemode_commits) = run("codemode", shape, n).await;
            let per = |value: f64| format!("{:.2}", value / n as f64);
            println!(
                "{shape} n={n}: plain {plain_ms:.0} ms ({} ms, {} commits/call), codemode {codemode_ms:.0} ms ({} ms, {} commits/call)",
                per(plain_ms),
                per(plain_commits as f64),
                per(codemode_ms),
                per(codemode_commits as f64),
            );
        }
    }
}
