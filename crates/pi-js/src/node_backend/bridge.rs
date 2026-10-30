//! The bridge thread: reads the child's messages and runs sync host functions, as the QuickJS
//! thread does. Blocking calls into JavaScript from this thread fail, as they do on QuickJS.

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};

use base64::Engine;
use serde_json::{Value, json};

use super::client::NodeClient;
use super::{modules, wire};
use crate::error::JsError;
use crate::host::HostFunction;
use crate::runtime::state::Shared;
use crate::runtime::{JsHandle, OutputStream, UncaughtKind};

pub(crate) struct Bridge {
    pub handle: JsHandle,
    pub client: Arc<NodeClient>,
    pub shared: Arc<Shared>,
    pub ready: Option<mpsc::Sender<Result<(), JsError>>>,
}

impl Bridge {
    pub fn run(mut self, socket: TcpStream) {
        let _ = self.client.bridge_thread.set(std::thread::current().id());
        let tokio = self.shared.tokio.clone();
        let _entered = tokio.enter();
        for line in BufReader::new(socket).lines() {
            let Ok(line) = line else { break };
            if let Ok(frame) = serde_json::from_str::<Value>(&line) {
                self.dispatch(&frame);
            }
        }
        self.shared.alive.store(false, Ordering::SeqCst);
        self.client.set_exit_reason("the Node extension runtime exited".into());
        self.client.fail_all();
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(Err(self.client.exited()));
        }
    }

    fn dispatch(&mut self, frame: &Value) {
        let id = &frame["id"];
        match frame["t"].as_str().unwrap_or_default() {
            "ready" => {
                self.shared.alive.store(true, Ordering::SeqCst);
                if let Some(ready) = self.ready.take() {
                    let _ = ready.send(Ok(()));
                }
            }
            "fatal" => {
                if let Some(ready) = self.ready.take() {
                    let _ = ready.send(Err(wire::error(&frame["error"])));
                }
            }
            "res" => {
                let answer = match frame.get("err") {
                    Some(error) => Err(wire::error(error)),
                    None => wire::decode(&self.handle, &frame["ok"]),
                };
                self.client.complete(id.as_u64().unwrap_or_default(), answer);
            }
            "host" => self.host_call(id, frame),
            "meta" => {
                let functions = self.shared.host_functions.read().unwrap_or_else(|poison| poison.into_inner());
                let answer = match frame["op"].as_str() {
                    Some("has") => json!(functions.get(frame["name"].as_str().unwrap_or_default()).is_some()),
                    _ => json!(functions.names()),
                };
                drop(functions);
                self.reply(id, Ok(answer));
            }
            "resolve" => {
                let parent = frame["parent"].as_str();
                let specifier = frame["specifier"].as_str().unwrap_or_default();
                let kind = frame["kind"].as_str().unwrap_or("import");
                self.reply(id, modules::resolve(&self.shared, &self.client, specifier, parent, kind));
            }
            "load" => {
                let path = frame["path"].as_str().unwrap_or_default();
                self.reply(id, modules::load(&self.shared, &self.client, path));
            }
            "out" => {
                let stream = if frame["s"] == 2 { OutputStream::Stderr } else { OutputStream::Stdout };
                let text = frame["b"].as_str().unwrap_or_default();
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(text) {
                    self.shared.output.write(stream, &bytes);
                }
            }
            "uncaught" => {
                let kind = if frame["kind"] == "rejection" { UncaughtKind::Rejection } else { UncaughtKind::Exception };
                deliver(&self.shared, kind, &wire::error(&frame["error"]));
            }
            "exit" => {
                let code = frame["code"].as_i64().unwrap_or(0) as i32;
                self.client.ending.store(true, Ordering::SeqCst);
                self.client.set_exit_reason(format!("the extension called process.exit({code})"));
                match &self.shared.config.on_exit {
                    Some(callback) => callback(code),
                    None => std::process::exit(code),
                }
                self.reply(id, Ok(Value::Null));
            }
            "ended" => {
                let code = frame["code"].as_i64().unwrap_or(0);
                self.client.set_exit_reason(format!("the Node worker exited with code {code}"));
                self.shared.alive.store(false, Ordering::SeqCst);
                self.client.fail_all();
                if let Some(ready) = self.ready.take() {
                    let _ = ready.send(Err(self.client.exited()));
                }
            }
            _ => {}
        }
    }

    fn reply(&self, id: &Value, result: Result<Value, JsError>) {
        let frame = match result {
            Ok(value) => json!({ "t": "reply", "id": id, "ok": value }),
            Err(error) => json!({ "t": "reply", "id": id, "err": wire::error_json(&error) }),
        };
        let _ = self.client.write(&frame);
    }

    fn host_call(&self, id: &Value, frame: &Value) {
        let name = frame["name"].as_str().unwrap_or_default();
        let function =
            self.shared.host_functions.read().unwrap_or_else(|poison| poison.into_inner()).get(name).cloned();
        let Some(function) = function else {
            let error = JsError::new(format!("Unknown host function: {name}")).with_code("ERR_PI_HOST_FUNCTION");
            return self.reply(id, Err(error));
        };
        let args = match wire::decode_all(&self.handle, frame.get("args")) {
            Ok(args) => args,
            Err(error) => return self.reply(id, Err(error)),
        };
        match function {
            HostFunction::Sync(function) => {
                let result = function(args).and_then(|value| wire::encode(&self.handle, &value));
                self.reply(id, result);
            }
            HostFunction::Async(function) => {
                let settle_id = self.client.next_id();
                let _ = self.client.write(&json!({ "t": "reply", "id": id, "async": settle_id }));
                let (handle, client) = (self.handle.clone(), self.client.clone());
                self.shared.tokio.spawn(async move {
                    let result = function(args).await.and_then(|value| wire::encode(&handle, &value));
                    let frame = match result {
                        Ok(value) => json!({ "t": "settle", "id": settle_id, "ok": value }),
                        Err(error) => json!({ "t": "settle", "id": settle_id, "err": wire::error_json(&error) }),
                    };
                    let _ = client.write(&frame);
                });
            }
        }
    }
}

pub(crate) fn deliver(shared: &Shared, kind: UncaughtKind, error: &JsError) {
    match &shared.config.on_uncaught {
        Some(callback) => callback(kind, error),
        None => {
            let prefix = if kind == UncaughtKind::Rejection { "Unhandled rejection: " } else { "Uncaught " };
            let text = format!("{prefix}{}\n", error.display_with_stack());
            shared.output.write(OutputStream::Stderr, text.as_bytes());
        }
    }
}
