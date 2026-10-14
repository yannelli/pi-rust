//! Coding tools (upstream `src/tools`): `read`, `write`, `edit`, `bash`, and `powershell`, built on the execution
//! environment of each call. `create_coding_tools` bundles the first four as the `coding-tools` extension.

mod api;
pub mod bash;
pub mod edit;
pub mod edit_diff;
pub mod file_mutation_queue;
pub mod image;
pub mod path_utils;
pub mod read;
pub mod write;

use std::sync::Arc;

use pi_chord::ChordError;

pub use api::{ImageModel, ToolCallApi, image_model_of};
pub use bash::{
    BashExecution, BashPrepare, BashToolOptions, PowerShellToolOptions, create_bash_tool, create_powershell_tool,
    execute_bash, execute_powershell,
};
pub use edit::{EditToolDetails, EditToolInput, create_edit_tool, execute_edit};
pub use read::{ReadToolOptions, create_read_tool, execute_read};
pub use write::{WriteToolInput, create_write_tool, execute_write};

use crate::env::SharedEnv;
use crate::errors::{DurableError, DurableResult};
use crate::harness::Extension;

/// The `coding-tools` extension: `read`, `write`, `edit`, and `bash`; nothing installs it automatically.
/// `create_powershell_tool()` adds `powershell`. `options.images` prepares the images `read` returns.
pub fn create_coding_tools(options: ReadToolOptions) -> Extension {
    Extension::new("coding-tools")
        .tool(create_read_tool(options))
        .tool(create_write_tool())
        .tool(create_edit_tool())
        .tool(create_bash_tool(BashToolOptions::default()))
}

/// `create_coding_tools()` without an image processor (upstream `CodingTools`).
pub fn coding_tools() -> Extension {
    create_coding_tools(ReadToolOptions::default())
}

/// The call's execution environment; a tool without one fails with an ordinary error result.
pub fn require_env(api: &dyn ToolCallApi) -> DurableResult<SharedEnv> {
    api.env().ok_or_else(|| DurableError::error("No execution environment is configured"))
}

/// A failure value thrown as the tool's error, keeping its type for callers that downcast it.
pub(crate) fn thrown(error: impl std::error::Error + Send + Sync + 'static) -> DurableError {
    DurableError::Chord(ChordError::Other(Arc::new(error)))
}

pub(crate) fn check_aborted(context: &pi_chord::Context) -> DurableResult<()> {
    if context.is_cancelled() { Err(DurableError::error("Operation aborted")) } else { Ok(()) }
}

/// Deserialize a tool's arguments, reporting a shape error as a `TypeError`.
pub(crate) fn parse_args<T: serde::de::DeserializeOwned>(args: pi_ai::JsonObject) -> DurableResult<T> {
    serde_json::from_value(serde_json::Value::Object(args)).map_err(|error| DurableError::type_error(error.to_string()))
}
