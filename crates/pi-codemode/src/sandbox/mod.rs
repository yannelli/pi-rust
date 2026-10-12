//! Runs JavaScript in an embedded QuickJS VM on its own thread. Scripts see `tools.<name>(args)`,
//! `ALL_TOOLS`, `text`, `image`, `exit`, `console.*`, `store`/`load`, and the configured globals;
//! nothing else (no timers, network, file system, or modules).

mod execution;
mod types;
mod worker;

use std::fmt;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

pub use types::*;

use crate::identifier::is_identifier;

/// Default deadline of one execution.
pub const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const RESERVED_GLOBALS: [&str; 9] =
    ["tools", "ALL_TOOLS", "console", "text", "image", "exit", "globalThis", "store", "load"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxError(pub String);

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SandboxError {}

#[derive(Clone, Debug)]
pub struct SandboxOptions {
    pub tools: Vec<SandboxFunction>,
    /// Functions exposed as top-level identifiers or `namespace.member`. The result's `calls` list
    /// leaves them out. Names may not shadow the built-in globals.
    pub globals: Vec<SandboxFunction>,
    /// Default deadline per execution.
    pub timeout: Timeout,
    /// Heap limit of the VM. Allocations beyond it throw `InternalError: out of memory` in the script.
    pub memory_limit_bytes: Option<usize>,
}

impl Default for SandboxOptions {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            globals: Vec::new(),
            timeout: Timeout::After(Duration::from_millis(DEFAULT_TIMEOUT_MS)),
            memory_limit_bytes: None,
        }
    }
}

/// Holds the tool table and defaults. Each [`execute`](Self::execute) gets its own thread and VM.
pub struct CodemodeSandbox {
    tools: Vec<SandboxFunction>,
    globals: Vec<SandboxFunction>,
    timeout: Timeout,
    memory_limit_bytes: Option<usize>,
    closed: CancellationToken,
}

impl CodemodeSandbox {
    pub fn new(options: SandboxOptions) -> Result<Self, SandboxError> {
        let mut sandbox = Self {
            tools: Vec::new(),
            globals: Vec::new(),
            timeout: options.timeout,
            memory_limit_bytes: options.memory_limit_bytes,
            closed: CancellationToken::new(),
        };
        for tool in options.tools {
            sandbox.register_tool(tool)?;
        }
        let mut namespaces = Vec::new();
        for global in options.globals {
            let parts: Vec<&str> = global.name().split('.').collect();
            if parts.len() > 2 || !parts.iter().all(|part| is_identifier(part)) || RESERVED_GLOBALS.contains(&parts[0])
            {
                return Err(SandboxError(format!("Invalid global name \"{}\"", global.name())));
            }
            if sandbox.globals.iter().any(|existing| existing.name() == global.name()) {
                return Err(SandboxError(format!("Global \"{}\" is already registered", global.name())));
            }
            if parts.len() == 2 {
                namespaces.push(parts[0].to_string());
            }
            sandbox.globals.push(global);
        }
        if let Some(name) = namespaces.iter().find(|name| sandbox.globals.iter().any(|global| global.name() == *name)) {
            return Err(SandboxError(format!("Global \"{name}\" conflicts with the namespace \"{name}\"")));
        }
        Ok(sandbox)
    }

    /// Fails if a tool with the same name is already registered.
    pub fn register_tool(&mut self, tool: SandboxFunction) -> Result<(), SandboxError> {
        if self.tools.iter().any(|existing| existing.name() == tool.name()) {
            return Err(SandboxError(format!("Tool \"{}\" is already registered", tool.name())));
        }
        self.tools.push(tool);
        Ok(())
    }

    pub fn unregister_tool(&mut self, name: &str) -> bool {
        let before = self.tools.len();
        self.tools.retain(|tool| tool.name() != name);
        self.tools.len() != before
    }

    pub fn tools(&self) -> &[SandboxFunction] {
        &self.tools
    }

    pub fn globals(&self) -> &[SandboxFunction] {
        &self.globals
    }

    /// `code` is an async function body: `return` and top-level `await` work. Script failures come
    /// back as a result with an error; only a closed sandbox fails the call.
    pub async fn execute(&self, code: &str, options: ExecuteOptions) -> Result<CodemodeResult, SandboxError> {
        if self.closed.is_cancelled() {
            return Err(SandboxError("Sandbox is closed".into()));
        }
        Ok(execution::run(execution::ExecutionParams {
            code: code.into(),
            tools: self.tools.clone(),
            globals: self.globals.clone(),
            timeout: options.timeout.unwrap_or(self.timeout),
            cancel: options.cancel,
            closed: self.closed.clone(),
            memory_limit: self.memory_limit_bytes,
            store: options.store,
        })
        .await)
    }

    /// Aborts in-flight executions (they fail with `kind: aborted`) and rejects new ones.
    pub fn close(&self) {
        self.closed.cancel();
    }
}
