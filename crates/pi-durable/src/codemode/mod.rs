//! Durable code mode (upstream `@earendil-works/pi-durable/codemode`): a `codemode` tool whose model-written scripts
//! call the conversation's tools as nested calls and survive a restart, by deterministic replay of a journal of the
//! results each script got, written before each delivery.

mod call;
mod calls;
mod describe;
mod details;
mod discover;
mod docs;
mod files;
mod model_checks;
mod models;
mod outcome;
mod output;
mod replay;
mod run;

use std::sync::Arc;

use futures::future::BoxFuture;
use pi_chord::Context;
use pi_codemode::{
    CodemodeModels, CodemodeRun, CodemodeRunOptions, CodemodeSandbox, SandboxError, codemode_constrained_sampling,
};
use serde_json::{Value, json};

pub use describe::{CodemodeNamespace, DEFAULT_CODEMODE_INLINE_BUDGET, codemode_description, codemode_section};
pub use details::{CodemodeNestedCall, CodemodeNestedCallStatus, CodemodeToolDetails};
pub use docs::{
    CODEMODE_JOURNAL_DOC, CODEMODE_RUN_DOC, CODEMODE_STORE_DOC, CodemodeStart, DeclaredGlobal, DeclaredNamespace,
    DeclaredTool, JournalEntry, JournalGlobal, JournalState, RunState, StoreState, StoredResult,
};
pub use models::{
    CLASSIFY_TOOL, CODEMODE_INTERNAL_PREFIX, GENERATE_IMAGES_TOOL, MODELS_DESCRIPTION, create_model_tools,
};

use crate::errors::{DurableError, DurableResult};
use crate::harness::{
    Extension, ToolCaller, ToolExecutionApi, ToolOutputLimits, ToolRegistration, ToolReplay, define_tool, section,
};

/// Version of everything in this module that turns stored results into what a script sees: result mapping, globals,
/// start data. Bump it when that changes, so a call started before an upgrade is not replayed by different code.
pub const CODEMODE_MODULE_VERSION: u32 = 2;

pub const CODEMODE_TOOL_NAME: &str = "codemode";

/// Runs scripts with pi-codemode's deterministic `start()` (upstream `CodemodeSandboxBase`).
pub trait CodemodeSandboxBase: Send + Sync {
    fn start(&self, code: &str, options: CodemodeRunOptions) -> Result<CodemodeRun, SandboxError>;
}

impl CodemodeSandboxBase for CodemodeSandbox {
    fn start(&self, code: &str, options: CodemodeRunOptions) -> Result<CodemodeRun, SandboxError> {
        CodemodeSandbox::start(self, code, options)
    }
}

/// Runs a global: the script's arguments (`None` for `undefined`); `Ok(None)` resolves to `undefined`.
pub type CodemodeGlobalFn = Arc<
    dyn Fn(Option<Value>, ToolExecutionApi, Context) -> BoxFuture<'static, DurableResult<Option<Value>>> + Send + Sync,
>;

/// A function scripts call as a top-level global; its result is journaled.
#[derive(Clone)]
pub struct CodemodeGlobal {
    pub name: String,
    pub spread: bool,
    pub execute: CodemodeGlobalFn,
}

impl CodemodeGlobal {
    pub fn new<F, Fut>(name: impl Into<String>, execute: F) -> Self
    where
        F: Fn(Option<Value>, ToolExecutionApi, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = DurableResult<Option<Value>>> + Send + 'static,
    {
        Self {
            name: name.into(),
            spread: false,
            execute: Arc::new(move |args, api, ctx| Box::pin(execute(args, api, ctx))),
        }
    }

    pub fn with_spread(mut self) -> Self {
        self.spread = true;
        self
    }
}

impl std::fmt::Debug for CodemodeGlobal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodemodeGlobal").field("name", &self.name).field("spread", &self.spread).finish()
    }
}

#[derive(Clone)]
pub struct CodemodeToolOptions {
    /// Runs the scripts, usually a [`CodemodeSandbox`].
    pub sandbox: Arc<dyn CodemodeSandboxBase>,
    /// Groups of tools by name prefix, such as MCP servers, for the `codemode` section and tool search.
    pub namespaces: Vec<CodemodeNamespace>,
    /// Estimated tokens the tool declarations in the `codemode` section may use. Default 3000.
    pub inline_budget: Option<usize>,
    /// Gives scripts `models.*`: the model catalog, classifiers, and image generation. `None`: no `models`.
    pub models: Option<Arc<dyn CodemodeModels>>,
    /// Host functions scripts call as globals.
    pub globals: Vec<CodemodeGlobal>,
}

impl CodemodeToolOptions {
    pub fn new(sandbox: Arc<dyn CodemodeSandboxBase>) -> Self {
        Self { sandbox, namespaces: Vec::new(), inline_budget: None, models: None, globals: Vec::new() }
    }
}

/// The result's limits: outputs are bounded by the script's own token budget; these only stop a runaway.
const OUTPUT_LIMITS: output::Limits = output::Limits { max_bytes: 4 * 1024 * 1024, max_lines: 100_000 };

fn codemode_parameters() -> Value {
    json!({
        "type": "object",
        "properties": { "code": { "type": "string", "description": "Raw JavaScript source." } },
        "required": ["code"],
    })
}

/// The code mode tool. Scripts call the conversation's callable tools as `tools.<name>(args)`.
pub fn create_codemode_tool(options: CodemodeToolOptions) -> ToolRegistration {
    let description = codemode_description(options.models.is_some());
    let options = Arc::new(options);
    let limits = ToolOutputLimits {
        max_bytes: Some(OUTPUT_LIMITS.max_bytes),
        max_lines: Some(OUTPUT_LIMITS.max_lines),
        retain: None,
    };
    let mut tool = define_tool(CODEMODE_TOOL_NAME, description, codemode_parameters(), move |args, api, context| {
        let options = options.clone();
        async move {
            let Some(Value::String(code)) = args.get("code") else {
                return Err(DurableError::type_error("codemode expects a code string"));
            };
            run::execute(options, api, context, code).await
        }
    })
    .with_output_limits(limits)
    .with_callers(vec![ToolCaller::Model])
    .with_replay(ToolReplay::Safe);
    // Capable models write the script as raw text instead of a JSON-escaped string; pi-ai applies it only for them.
    tool.tool.constrained_sampling = Some(codemode_constrained_sampling());
    tool
}

/// The `codemode` extension: the code mode tool, its internal model tools when scripts get `models.*`, and the
/// `codemode` prompt section listing the tools scripts can call.
pub fn create_codemode_extension(options: CodemodeToolOptions) -> Extension {
    let (namespaces, budget) = (options.namespaces.clone(), options.inline_budget);
    let mut extension = Extension::new("codemode").tool(create_codemode_tool(options.clone()));
    for tool in options.models.map(create_model_tools).unwrap_or_default() {
        extension = extension.tool(tool);
    }
    let render = move |input: crate::harness::PromptInput, _| {
        // Deferred tools are found with `searchTools()`, as the coding agent leaves them out too.
        let listed =
            |tool: &&Arc<ToolRegistration>| !tool.name().starts_with(CODEMODE_INTERNAL_PREFIX) && !tool.deferred;
        let callable: Vec<Arc<ToolRegistration>> = input.agent.callable.iter().filter(listed).cloned().collect();
        let budget = budget.unwrap_or(DEFAULT_CODEMODE_INLINE_BUDGET);
        std::future::ready(Ok(codemode_section(&callable, &namespaces, budget)))
    };
    extension.section(section("codemode", render, None))
}
