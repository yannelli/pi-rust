//! The code mode call's details: the script's tool calls with their status, written once with the result.

use std::collections::BTreeMap;

use pi_chord::Json;
use pi_codemode::CodemodeHostCall;
use pi_codemode::js::{js_stringify, utf16_len, utf16_slice};
use serde::{Deserialize, Serialize};

use super::models::CODEMODE_INTERNAL_PREFIX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeNestedCallStatus {
    Running,
    Ok,
    Error,
    Cancelled,
}

/// One tool call a script made, as the code mode call's details show it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeNestedCall {
    /// The VM's number for the call; its nested call ID is `<callId>/c<id>`.
    pub id: u64,
    pub name: String,
    /// Compact JSON of the arguments, cut for display.
    pub args: String,
    pub status: CodemodeNestedCallStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Error text, cut for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The code mode call's details: its tool calls in the order it made them, with their status. Calls to globals are
/// not listed. While the script runs, UIs show its nested calls from `pi.live.nestedTools`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeToolDetails {
    pub calls: Vec<CodemodeNestedCall>,
    /// File with the complete text output, when the output was cut to its token budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

const ARGS_PREVIEW_CHARS: usize = 200;
const ERROR_PREVIEW_CHARS: usize = 500;

fn cut(text: &str, max: usize) -> String {
    if utf16_len(text) > max { format!("{}...", utf16_slice(text, 0, max - 3)) } else { text.to_owned() }
}

/// The script's tool calls for `details`, by VM call ID.
#[derive(Default)]
pub(super) struct Rows(BTreeMap<u64, CodemodeNestedCall>);

impl Rows {
    /// The script made a tool call: its row appears, running, also while it waits to launch.
    pub fn started(&mut self, call: &CodemodeHostCall) {
        let args = call.args.as_ref().map(js_stringify).unwrap_or_default();
        let name = call.name.strip_prefix(CODEMODE_INTERNAL_PREFIX).unwrap_or(&call.name);
        self.0.entry(call.id).or_insert_with(|| CodemodeNestedCall {
            id: call.id,
            name: name.to_owned(),
            args: cut(&args, ARGS_PREVIEW_CHARS),
            status: CodemodeNestedCallStatus::Running,
            duration_ms: None,
            error: None,
        });
    }

    /// A tool call ended: an error result is an error row, also when the script got its value; `error` says why.
    pub fn finish(&mut self, id: u64, is_error: bool, error: &str, duration_ms: Option<u64>) {
        let Some(row) = self.0.get_mut(&id) else { return };
        row.status = if is_error { CodemodeNestedCallStatus::Error } else { CodemodeNestedCallStatus::Ok };
        if duration_ms.is_some() {
            row.duration_ms = duration_ms;
        }
        if is_error && !error.is_empty() {
            row.error = Some(cut(error, ERROR_PREVIEW_CHARS));
        }
    }

    /// The rows in call order; calls still running when the script ended were cancelled.
    pub fn calls(&self) -> Vec<CodemodeNestedCall> {
        let calls = self.0.values().cloned().map(|mut row| {
            if row.status == CodemodeNestedCallStatus::Running {
                row.status = CodemodeNestedCallStatus::Cancelled;
            }
            row
        });
        calls.collect()
    }

    pub fn details(&self) -> Json {
        CodemodeToolDetails { calls: self.calls(), full_output_path: None }.to_json()
    }
}

impl CodemodeToolDetails {
    pub(super) fn to_json(&self) -> Json {
        Json::from(serde_json::to_value(self).unwrap_or_default())
    }
}
