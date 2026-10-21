//! The code mode call's details: the script's tool calls with their status, written once with the result.

use std::collections::BTreeMap;

use pi_chord::Json;
use pi_codemode::js::{js_stringify, utf16_len, utf16_slice};
use pi_codemode::{CodemodeHostCall, CodemodeOutcome};
use serde::{Deserialize, Serialize};

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
pub struct CodemodeToolDetails {
    pub calls: Vec<CodemodeNestedCall>,
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
        self.0.entry(call.id).or_insert_with(|| CodemodeNestedCall {
            id: call.id,
            name: call.name.clone(),
            args: cut(&args, ARGS_PREVIEW_CHARS),
            status: CodemodeNestedCallStatus::Running,
            duration_ms: None,
            error: None,
        });
    }

    /// A tool call ended: its row gets its status, duration, and error.
    pub fn finish(&mut self, id: u64, outcome: &CodemodeOutcome, duration_ms: Option<u64>) {
        let Some(row) = self.0.get_mut(&id) else { return };
        match outcome {
            CodemodeOutcome::Ok(_) => row.status = CodemodeNestedCallStatus::Ok,
            CodemodeOutcome::Err { message, .. } => {
                row.status = CodemodeNestedCallStatus::Error;
                row.error = Some(cut(message, ERROR_PREVIEW_CHARS));
            }
        }
        if duration_ms.is_some() {
            row.duration_ms = duration_ms;
        }
    }

    /// The rows in call order; calls still running when the script ended were cancelled.
    pub fn details(&self) -> Json {
        let calls = self.0.values().cloned().map(|mut row| {
            if row.status == CodemodeNestedCallStatus::Running {
                row.status = CodemodeNestedCallStatus::Cancelled;
            }
            row
        });
        let details = CodemodeToolDetails { calls: calls.collect() };
        Json::from(serde_json::to_value(details).unwrap_or_default())
    }
}
