//! Nested call rows of a `codemode` result: tool calls and `models.*` calls as the script runs them.

use std::sync::{Arc, Mutex};

use pi_agent::{AgentToolResult, ToolUpdateCallback};
use pi_ai::{Cost, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::js::{js_stringify, utf16_len, utf16_slice};

pub(crate) const ARGS_PREVIEW_CHARS: usize = 200;
pub(crate) const ERROR_PREVIEW_CHARS: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeNestedCallStatus {
    Running,
    Ok,
    Error,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeNestedCall {
    /// Tool call id of the nested call, `<codemode call id>/<n>`.
    pub id: String,
    pub name: String,
    /// Compact JSON of the arguments, truncated for display.
    pub args: String,
    pub status: CodemodeNestedCallStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    /// Error text, truncated for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Cost in USD of a `models.*` call that reported usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

/// `details` of a `codemode` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeToolDetails {
    pub calls: Vec<CodemodeNestedCall>,
    /// Temp file with the full text output, when the output was truncated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

pub(crate) fn truncate_text(text: &str, max_chars: usize) -> String {
    if utf16_len(text) > max_chars { format!("{}...", utf16_slice(text, 0, max_chars - 3)) } else { text.into() }
}

pub(crate) fn preview_args(args: Option<&Value>) -> String {
    args.map(|args| truncate_text(&js_stringify(args), ARGS_PREVIEW_CHARS)).unwrap_or_default()
}

/// The rows of one script, shared by its tool and model calls, published on every change.
pub(crate) struct CallLog {
    calls: Mutex<Vec<CodemodeNestedCall>>,
    on_update: Option<ToolUpdateCallback>,
}

impl CallLog {
    pub fn new(on_update: Option<ToolUpdateCallback>) -> Arc<Self> {
        Arc::new(Self { calls: Mutex::new(Vec::new()), on_update })
    }

    pub fn push(&self, call: CodemodeNestedCall) -> usize {
        let index = {
            let mut calls = self.calls.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            calls.push(call);
            calls.len() - 1
        };
        self.publish();
        index
    }

    pub fn update(&self, index: usize, change: impl FnOnce(&mut CodemodeNestedCall)) {
        {
            let mut calls = self.calls.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(call) = calls.get_mut(index) {
                change(call);
            }
        }
        self.publish();
    }

    pub fn snapshot(&self) -> Vec<CodemodeNestedCall> {
        self.calls.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// Calls still running were cut off by the script ending, a timeout, or an abort.
    pub fn cancel_running(&self) {
        for call in self.calls.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter_mut() {
            if call.status == CodemodeNestedCallStatus::Running {
                call.status = CodemodeNestedCallStatus::Cancelled;
            }
        }
    }

    /// Send the current rows to the tool's update callback.
    pub fn publish(&self) {
        if let Some(on_update) = &self.on_update {
            let details = CodemodeToolDetails { calls: self.snapshot(), full_output_path: None };
            let details = serde_json::to_value(details).unwrap_or(Value::Null);
            on_update(AgentToolResult { content: Vec::new(), details: Some(details), ..AgentToolResult::default() });
        }
    }
}

/// Usage from a model result, filling fields the service did not report with zero.
pub(crate) fn usage_from_value(value: &Value) -> Option<Usage> {
    let object = value.as_object()?;
    let count = |key: &str| object.get(key).and_then(Value::as_f64).unwrap_or(0.0) as u64;
    let cost = object.get("cost").and_then(Value::as_object);
    let price = |key: &str| cost.and_then(|cost| cost.get(key)).and_then(Value::as_f64).unwrap_or(0.0);
    Some(Usage {
        input: count("input"),
        output: count("output"),
        cache_read: count("cacheRead"),
        cache_write: count("cacheWrite"),
        cache_write_1h: object.get("cacheWrite1h").and_then(Value::as_f64).map(|count| count as u64),
        reasoning: object.get("reasoning").and_then(Value::as_f64).map(|count| count as u64),
        total_tokens: count("totalTokens"),
        cost: Cost {
            input: price("input"),
            output: price("output"),
            cache_read: price("cacheRead"),
            cache_write: price("cacheWrite"),
            total: price("total"),
        },
    })
}

pub use pi_agent::combine_usage;
