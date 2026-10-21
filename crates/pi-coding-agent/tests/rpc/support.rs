//! In-process RPC mode over the faux provider: commands go in through a pipe, records come out
//! of an in-memory sink (upstream `rpc-prompt-response-semantics.test.ts` harness).

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_coding_agent::agent_session::AgentSessionRuntime;
use pi_coding_agent::app::AppOptions;
use pi_coding_agent::rpc::{RpcModeOptions, parse_json_line, serve_rpc};
use pi_coding_agent::session::SessionManager;
use serde_json::Value;
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::task::JoinHandle;

use crate::harness::{Harness, Options, harness};

#[derive(Clone, Default)]
pub struct Sink(pub Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct Rpc {
    pub h: Harness,
    pub runtime: Arc<AgentSessionRuntime>,
    input: Option<DuplexStream>,
    pub output: Sink,
    task: Option<JoinHandle<i32>>,
}

pub async fn start(options: Options) -> Rpc {
    start_with(options, RpcModeOptions::default(), |_, _| {}).await
}

/// Start with RPC options; `prepare` can change files and app options before the runtime starts.
pub async fn start_with(options: Options, rpc: RpcModeOptions, prepare: impl FnOnce(&Harness, &mut AppOptions)) -> Rpc {
    let h = harness(options).await;
    let mut app = h.app_options();
    prepare(&h, &mut app);
    let manager = SessionManager::create(&h.cwd, Some(&h.agent_dir.join("sessions")));
    let runtime = Arc::new(crate::offline_guard::app_runtime(app, manager).await);
    let (client, server) = tokio::io::duplex(1 << 16);
    let output = Sink::default();
    let task = tokio::spawn(serve_rpc(runtime.clone(), server, Box::new(output.clone()), rpc));
    Rpc { h, runtime, input: Some(client), output, task: Some(task) }
}

impl Rpc {
    /// Write raw bytes to the agent's stdin.
    pub async fn write(&mut self, text: &str) {
        let input = self.input.as_mut().expect("input open");
        input.write_all(text.as_bytes()).await.unwrap();
        input.flush().await.unwrap();
    }

    pub async fn send(&mut self, record: Value) {
        self.write(&format!("{record}\n")).await;
    }

    /// Complete output lines as text.
    pub fn lines(&self) -> Vec<String> {
        let bytes = self.output.0.lock().unwrap().clone();
        let text = String::from_utf8(bytes).unwrap();
        let complete = match text.rfind('\n') {
            Some(end) => &text[..end],
            None => "",
        };
        complete.split('\n').filter(|line| !line.is_empty()).map(String::from).collect()
    }

    pub fn records(&self) -> Vec<Value> {
        self.lines().iter().map(|line| parse_json_line(line).unwrap()).collect()
    }

    pub fn records_of(&self, kind: &str) -> Vec<Value> {
        self.records().into_iter().filter(|record| record["type"] == kind).collect()
    }

    pub fn responses(&self, id: &str) -> Vec<Value> {
        self.records().into_iter().filter(|record| record["type"] == "response" && record["id"] == id).collect()
    }

    /// The first record matching `predicate`, waiting up to ten seconds.
    pub async fn wait_for(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..2000 {
            if let Some(record) = self.records().into_iter().find(|record| predicate(record)) {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no matching record; output:\n{}", self.lines().join("\n"));
    }

    pub async fn response(&self, id: &str) -> Value {
        self.wait_for(|record| record["type"] == "response" && record["id"] == id).await
    }

    /// Send `record` (which has an `id`) and return its response.
    pub async fn request(&mut self, record: Value) -> Value {
        let id = record["id"].as_str().expect("request id").to_string();
        self.send(record).await;
        self.response(&id).await
    }

    /// Send `record` and return its `data`, asserting success.
    pub async fn data(&mut self, record: Value) -> Value {
        let response = self.request(record).await;
        assert_eq!(response["success"], true, "{response}");
        response.get("data").cloned().unwrap_or(Value::Null)
    }

    /// Wait for `agent_settled` number `count`.
    pub async fn settled(&self, count: usize) -> Value {
        for _ in 0..2000 {
            if let Some(record) = self.records_of("agent_settled").into_iter().nth(count - 1) {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("agent_settled #{count} missing; output:\n{}", self.lines().join("\n"));
    }

    /// Close stdin and return the exit code.
    pub async fn finish(&mut self) -> i32 {
        self.input.take();
        let task = self.task.take().expect("still running");
        tokio::time::timeout(Duration::from_secs(10), task).await.expect("RPC mode exits").unwrap()
    }

    /// Wait for the exit code without closing stdin.
    pub async fn exit_code(&mut self) -> i32 {
        let task = self.task.take().expect("still running");
        tokio::time::timeout(Duration::from_secs(10), task).await.expect("RPC mode exits").unwrap()
    }
}
