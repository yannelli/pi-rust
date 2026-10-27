//! Handle passing over IPC (`subprocess.send(message, socket)`) and `node:cluster` with two
//! workers sharing one TCP server. Children run this test binary as `node`.

mod common;

use common::*;
use serde_json::json;

const CHILD: &str = r#"
process.on("message", (message, handle) => {
  if (message === "socket") {
    // Greet first: the client writes only after this, so the parent never reads its bytes.
    handle.write("child:");
    handle.once("data", (data) => handle.end(String(data)));
  } else if (message === "server") {
    handle.on("connection", (socket) => socket.end("from child server"));
    process.send("ready");
  } else if (message === "bye") {
    process.disconnect();
    process.exit(0);
  }
});
"#;

const WORKER: &str = r#"
const cluster = require("cluster");
const http = require("http");
http.createServer((req, res) => res.end(`worker ${cluster.worker.id}`)).listen(0, "127.0.0.1");
"#;

async fn cases() {
    let (_dir, root) = tempdir();
    let capture = std::sync::Arc::new(Capture::default());
    let _ = STAGE.set(root.join("stage"));
    let _ = CAPTURE.set(capture.clone());
    let mut options = options(&root, &capture);
    let mut env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    env.insert(GUARD.into(), "1".into());
    options.env = Some(env);
    let runtime = pi_js::JsRuntime::new(options).unwrap();
    write(&root, "child.js", CHILD);
    write(&root, "worker.js", WORKER);
    let actual = run_cjs(
        &runtime,
        &root,
        r#"
const { fork } = require("child_process");
const net = require("net");
const http = require("http");
const path = require("path");
const out = {};
const mark = (text) => require("fs").appendFileSync(path.join(__dirname, "stage"), text + "\n");
const child = fork(path.join(__dirname, "child.js"));
const server = net.createServer((socket) => child.send("socket", socket));
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const talk = (port, text) => new Promise((resolve, reject) => {
  const client = net.connect(port, "127.0.0.1");
  let data = "";
  client.on("data", (chunk) => {
    if (text && !data) client.write(text);
    data += chunk;
  });
  client.on("end", () => resolve(data));
  client.on("error", reject);
});
mark('socket');
out.socket = await talk(server.address().port, "ping");
server.close();
const shared = net.createServer(() => {});
await new Promise((resolve) => shared.listen(0, "127.0.0.1", resolve));
const port = shared.address().port;
child.send("server", shared);
mark('server');
await new Promise((resolve) => child.once("message", resolve));
mark('ready');
shared.close();
mark('talk server');
out.server = await talk(port, "");
child.send("bye");
await new Promise((resolve) => child.once("exit", resolve));

const cluster = require("cluster");
out.primary = [cluster.isPrimary, cluster.isWorker, cluster.schedulingPolicy === cluster.SCHED_RR];
cluster.setupPrimary({ exec: path.join(__dirname, "worker.js") });
const events = [];
cluster.on("fork", (worker) => events.push(`fork ${worker.id}`));
cluster.on("online", (worker) => events.push(`online ${worker.id}`));
const listening = new Promise((resolve) => {
  const ports = [];
  cluster.on("listening", (worker, address) => {
    ports.push(address.port);
    if (ports.length === 2) resolve(ports);
  });
});
cluster.fork();
cluster.fork();
mark('cluster');
const ports = await listening;
mark('listening');
out.samePort = ports[0] === ports[1] && ports[0] > 0;
const get = () => new Promise((resolve, reject) => {
  http.get({ host: "127.0.0.1", port: ports[0], agent: false }, (res) => {
    let body = "";
    res.on("data", (chunk) => (body += chunk));
    res.on("end", () => resolve(body));
  }).on("error", reject);
});
const answers = new Set();
for (let i = 0; i < 6; i++) answers.add(await get());
out.answers = [...answers].sort();
const exits = new Promise((resolve) => {
  const codes = [];
  cluster.on("exit", (worker, code) => {
    codes.push([worker.id, code, worker.exitedAfterDisconnect]);
    if (codes.length === 2) resolve(codes.sort());
  });
});
mark('disconnect');
cluster.disconnect();
out.exits = await exits;
out.events = events.sort();
out.workersLeft = Object.keys(cluster.workers).length;
return out;
"#,
    )
    .await;
    assert_eq!(
        actual,
        json!({
            "socket": "child:ping",
            "server": "from child server",
            "primary": [true, false, true],
            "samePort": true,
            "answers": ["worker 1", "worker 2"],
            "exits": [[1, 0, true], [2, 0, true]],
            "events": ["fork 1", "fork 2", "online 1", "online 2"],
            "workersLeft": 0
        })
    );
}

/// Set for children, so one that is not asked to run JavaScript cannot rerun the tests.
const GUARD: &str = "PI_JS_FORK_TEST_CHILD";

static STAGE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
static CAPTURE: std::sync::OnceLock<std::sync::Arc<Capture>> = std::sync::OnceLock::new();

fn main() {
    if let Some(code) = pi_js::runner::child_main() {
        std::process::exit(code);
    }
    if std::env::var_os(GUARD).is_some() {
        std::process::exit(97);
    }
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(if std::env::var_os("PI_QUICK").is_some() {
            20
        } else {
            240
        }));
        let stage = STAGE.get().and_then(|path| std::fs::read_to_string(path).ok()).unwrap_or_default();
        let (out, err) = CAPTURE.get().map(|capture| (capture.stdout(), capture.stderr())).unwrap_or_default();
        eprintln!("test cluster_cases ... FAILED: timed out at: {stage}\nstdout: {out}\nstderr: {err}");
        std::process::exit(101);
    });
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let (out, err) = CAPTURE.get().map(|capture| (capture.stdout(), capture.stderr())).unwrap_or_default();
        eprintln!("stdout: {out}\nstderr: {err}");
        default_hook(info);
    }));
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(cases());
    println!("test cluster_cases ... ok\n\ntest result: ok. 1 passed; 0 failed");
}
