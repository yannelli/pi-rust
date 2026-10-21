//! `--output-schema` (pi-rust extension): in print mode the final answer must be a JSON value
//! that matches a JSON Schema. pi adds the schema to the system prompt, checks the last response
//! with `pi-schema`, and sends corrective prompts with the errors, up to `--output-retries`.

mod support;

use std::path::Path;

use serde_json::Value;

use crate::cli::Args;
use crate::resources::paths::resolve_path;

/// Exit code of print and JSON mode when the final response does not match the schema.
pub const OUTPUT_SCHEMA_EXIT_CODE: i32 = 4;

/// Corrective prompts when `--output-retries` is not given.
pub const DEFAULT_OUTPUT_RETRIES: u32 = 1;

/// The schema of `--output-schema`, loaded and checked, with `--output-retries`.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputSchema {
    /// The `--output-schema` value as given.
    pub path: String,
    pub schema: Value,
    /// The `--output-retries` value, if given.
    pub retries_flag: Option<u32>,
}

impl OutputSchema {
    /// `--output-schema` and `--output-retries` of `args`, with `print_mode` true for `-p`,
    /// `--mode json`, and redirected streams. Fails on a bad flag combination or schema file.
    pub fn from_args(args: &Args, cwd: &Path, print_mode: bool) -> Result<Option<Self>, String> {
        let Some(path) = &args.output_schema else {
            return match args.output_retries {
                Some(_) => Err("--output-retries requires --output-schema".into()),
                None => Ok(None),
            };
        };
        if !print_mode {
            return Err("--output-schema requires print mode (-p or --mode json)".into());
        }
        Self::load(path, cwd, args.output_retries).map(Some)
    }

    /// Read and check the schema file at `path`, relative to `cwd`.
    pub fn load(path: &str, cwd: &Path, retries_flag: Option<u32>) -> Result<Self, String> {
        let file = resolve_path(path, cwd);
        let text = std::fs::read_to_string(&file)
            .map_err(|error| format!("--output-schema: cannot read {}: {error}", file.display()))?;
        let schema: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|error| format!("--output-schema: {path} is not valid JSON: {error}"))?;
        let problems = support::unsupported(&schema);
        if !problems.is_empty() {
            let list: String = problems.iter().map(|problem| format!("\n  {problem}")).collect();
            return Err(format!("--output-schema: {path} is not a supported JSON Schema:{list}"));
        }
        Ok(Self { path: path.to_string(), schema, retries_flag })
    }

    /// The schema that applies when `registered` tells which flags extensions registered: none
    /// when one registered `--output-schema`, the default retries for `--output-retries`.
    pub fn unclaimed(&self, registered: impl Fn(&str) -> bool) -> Option<Self> {
        let retries_flag = self.retries_flag.filter(|_| !registered("output-retries"));
        (!registered("output-schema")).then(|| Self { retries_flag, ..self.clone() })
    }

    /// How many corrective prompts to send at most.
    pub fn retries(&self) -> u32 {
        self.retries_flag.unwrap_or(DEFAULT_OUTPUT_RETRIES)
    }

    /// The text appended to the system prompt.
    pub fn instruction(&self) -> String {
        format!(
            "# Output format\n\nYour final message must be only a JSON value that matches the JSON Schema below, \
             with no other text and no Markdown code fence. Use tools as usual before it.\n\n{}",
            serde_json::to_string_pretty(&self.schema).unwrap_or_default()
        )
    }

    /// The JSON value of a response, or the errors to report: invalid JSON, or one
    /// `path: message` line per schema error.
    pub fn check(&self, text: &str) -> Result<Value, Vec<String>> {
        let value: Value = serde_json::from_str(strip_fence(text))
            .map_err(|error| vec![format!("the response is not valid JSON: {error}")])?;
        let errors = pi_ai::json_schema::schema_errors(&self.schema, &value);
        if errors.is_empty() { Ok(value) } else { Err(errors) }
    }
}

/// The text inside a single surrounding Markdown code fence (```` ```json ```` or ```` ``` ````),
/// or the whole text when it has no such fence.
pub fn strip_fence(text: &str) -> &str {
    let text = text.trim();
    let Some((info, body)) = text.strip_prefix("```").and_then(|rest| rest.split_once('\n')) else { return text };
    let info = info.trim();
    if !info.is_empty() && !info.eq_ignore_ascii_case("json") {
        return text;
    }
    match body.trim_end().strip_suffix("```") {
        Some(inner) if !inner.contains("```") => inner.trim(),
        _ => text,
    }
}

/// The follow-up prompt that asks for corrected JSON.
pub fn correction_prompt(errors: &[String]) -> String {
    let list: String = errors.iter().map(|error| format!("\n- {error}")).collect();
    format!(
        "Your final message does not match the required JSON Schema:{list}\n\n\
         Reply with only the corrected JSON value, with no other text and no code fence."
    )
}

#[cfg(test)]
mod tests;
