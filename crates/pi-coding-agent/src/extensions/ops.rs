//! Host operations JavaScript calls (`ext.sync` and `ext.async`): the first argument names the
//! operation, the rest are its JSON arguments. Most take a session target first: 0 is the
//! runner's own session, other ids are sessions that replaced it (`withSession`).

use std::sync::Arc;

use pi_js::{HostValue, JsError};
use serde_json::Value;

use super::runner::ExtensionRunner;
use super::{ops_async, ops_host, ops_session, ops_ui};
use crate::agent_session::{AgentSession, lock};
use crate::extension_ui::ExtensionUi;

/// The arguments after the operation name.
pub(crate) struct Args(pub(crate) Vec<HostValue>);

impl Args {
    pub fn json(&self, index: usize) -> Value {
        self.0.get(index).cloned().map(HostValue::into_json).unwrap_or(Value::Null)
    }

    pub fn str(&self, index: usize) -> Result<String, JsError> {
        match self.json(index) {
            Value::String(text) => Ok(text),
            other => Err(JsError::type_error(format!("Expected a string argument, got {other}"))),
        }
    }

    pub fn opt_str(&self, index: usize) -> Option<String> {
        self.json(index).as_str().map(str::to_string)
    }

    pub fn u64(&self, index: usize) -> u64 {
        self.json(index).as_u64().unwrap_or(0)
    }

    pub fn bool(&self, index: usize) -> bool {
        self.json(index).as_bool().unwrap_or(false)
    }
}

pub(crate) fn error(message: impl std::fmt::Display) -> JsError {
    JsError::new(message.to_string())
}

fn split(args: Vec<HostValue>) -> Result<(String, Args), JsError> {
    let mut args = args.into_iter();
    let op = args.next().and_then(|value| value.as_str().map(str::to_string));
    let op = op.ok_or_else(|| JsError::type_error("Missing host operation name"))?;
    Ok((op, Args(args.collect())))
}

impl ExtensionRunner {
    /// The session of `target`.
    pub(crate) fn target_session(&self, target: u64) -> Result<Arc<AgentSession>, JsError> {
        let session = if target == 0 {
            self.session()
        } else {
            lock(&self.targets).get(&target).and_then(std::sync::Weak::upgrade)
        };
        session.ok_or_else(|| {
            error("Extension runtime not initialized. Action methods cannot be called during extension loading.")
        })
    }

    /// The UI of `target`: this runner's binding, or the binding of the replacing session.
    pub(crate) fn target_ui(&self, target: u64) -> Arc<dyn ExtensionUi> {
        if target == 0 {
            return self.bindings().ui;
        }
        let runner = self.target_session(target).ok().and_then(|session| session.extension_runner());
        runner.map(|runner| runner.bindings().ui).unwrap_or_else(|| self.bindings().ui)
    }

    /// The runner of `target`, for operations the replacing session's extensions handle.
    pub(crate) fn target_runner(&self, target: u64) -> Arc<ExtensionRunner> {
        if target == 0 {
            return self.this();
        }
        let runner = self.target_session(target).ok().and_then(|session| session.extension_runner());
        runner.unwrap_or_else(|| self.this())
    }
}

pub(crate) fn dispatch_sync(runner: &ExtensionRunner, args: Vec<HostValue>) -> Result<HostValue, JsError> {
    let (op, args) = split(args)?;
    let (group, name) = op.split_once('.').unwrap_or((op.as_str(), ""));
    match group {
        "sdk" => super::ops_sdk::sync(runner, name, &args),
        "storage" => super::ops_sdk_storage::sync(runner, &runner.sdk.storage, &op, &args),
        "resources" => super::ops_sdk_resources::sync(runner, name, &args),
        "ui" => ops_ui::sync(runner, name, &args),
        "ctx" | "session" | "models" | "tools" | "tool" | "action" | "prompt" | "boundary" => {
            ops_session::sync(runner, &op, &args)
        }
        _ => ops_host::sync(runner, &op, &args),
    }
}

pub(crate) async fn dispatch_async(runner: Arc<ExtensionRunner>, args: Vec<HostValue>) -> Result<HostValue, JsError> {
    let (op, args) = split(args)?;
    if op == "resources.reload" {
        return super::ops_sdk_resources::reload(runner, args).await;
    }
    if let Some(name) = op.strip_prefix("sdk.") {
        return super::ops_sdk::run(runner, name, args).await;
    }
    ops_async::run(runner, &op, args.0).await
}
