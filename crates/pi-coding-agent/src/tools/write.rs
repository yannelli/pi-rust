//! `write` tool: create or overwrite a file, creating parent directories.

use std::path::PathBuf;

use anyhow::Context;
use async_trait::async_trait;
use pi_agent::{AgentTool, AgentToolResult, ToolUpdateCallback};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::mutation_queue::with_file_mutation_queue;
use super::path_utils::resolve_to_cwd;
use super::{check_aborted, parse_args};

pub struct WriteTool {
    cwd: PathBuf,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

impl WriteTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
                "content": { "type": "string", "description": "Content to write to the file" }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        let args: WriteArgs = parse_args("write", args)?;
        let absolute = resolve_to_cwd(&args.path, &self.cwd);
        with_file_mutation_queue(&absolute, || async {
            check_aborted(&cancel)?;
            if let Some(dir) = absolute.parent() {
                tokio::fs::create_dir_all(dir)
                    .await
                    .with_context(|| format!("Could not create directory {}", dir.display()))?;
            }
            check_aborted(&cancel)?;
            tokio::fs::write(&absolute, args.content.as_bytes())
                .await
                .with_context(|| format!("Could not write file {}", absolute.display()))?;
            check_aborted(&cancel)?;
            let bytes = args.content.len();
            Ok(AgentToolResult::text(format!("Successfully wrote {bytes} bytes to {}", args.path)))
        })
        .await
    }
}
