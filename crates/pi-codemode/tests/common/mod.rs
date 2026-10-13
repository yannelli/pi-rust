#![allow(dead_code)]

use std::time::Duration;

use pi_codemode::{CodemodeResult, CodemodeSandbox, ExecuteOptions, SandboxFunction, SandboxOptions, Timeout};
use serde_json::Value;

/// Base64 of the leading bytes of each format. `image()` only inspects the signature.
pub const PNG: &str = "iVBORw0KGgo=";
pub const JPEG: &str = "/9j/4A==";
pub const GIF: &str = "R0lGODlh";
pub const WEBP: &str = "UklGRgAAAABXRUJQ";

pub fn sandbox(tools: Vec<SandboxFunction>) -> CodemodeSandbox {
    with_globals(tools, Vec::new())
}

pub fn with_globals(tools: Vec<SandboxFunction>, globals: Vec<SandboxFunction>) -> CodemodeSandbox {
    CodemodeSandbox::new(SandboxOptions {
        tools,
        globals,
        timeout: Timeout::After(Duration::from_secs(10)),
        ..SandboxOptions::default()
    })
    .expect("valid sandbox")
}

pub fn echo() -> SandboxFunction {
    SandboxFunction::new("echo", |args, _| async move { Ok(args) })
}

pub fn constant(name: &str, value: Value) -> SandboxFunction {
    SandboxFunction::new(name, move |_, _| {
        let value = value.clone();
        async move { Ok(Some(value)) }
    })
}

pub async fn run(sandbox: &CodemodeSandbox, code: &str) -> CodemodeResult {
    sandbox.execute(code, ExecuteOptions::default()).await.expect("open sandbox")
}

/// The return value, or the error message of a failed script.
pub async fn attempt(sandbox: &CodemodeSandbox, code: &str) -> Value {
    let result = run(sandbox, code).await;
    match result.outcome {
        Ok(success) => success.value.unwrap_or(Value::Null),
        Err(error) => Value::String(error.message),
    }
}
