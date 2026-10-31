//! What a report to `HarnessOptions::on_report` is about.

use serde::Serialize;

use crate::ids::{ConversationId, TaskId};
use crate::types::TaskOutcomeError;

/// The task a report is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportTask {
    pub id: TaskId,
    /// The task's registered name.
    pub kind: String,
    pub conversation_id: ConversationId,
}

/// A host callback, by its upstream `HarnessOptions` name; `env` is the environment builder, `models` a provider call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HostCallback {
    Now,
    Settings,
    Registry,
    Env,
    Models,
    SetTimer,
    OnWake,
}

/// Why a task ended `faulted`: its phase threw, or its abort handler was cut off `settings.restart_limit` times.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultReason {
    Threw,
    RestartLimit,
}

/// Why crashes ended a built-in task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptionReason {
    RestartLimit,
    Interrupted,
}

impl InterruptionReason {
    pub(crate) fn from_error(error: &TaskOutcomeError) -> Option<Self> {
        match error.detail.as_ref()?.get("reason")?.as_str()? {
            "restart_limit" => Some(Self::RestartLimit),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }
}

/// Why a running task keeps running under its old definition: it was uninstalled, or replaced by one that cannot
/// take over its state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutdatedReason {
    MissingTask,
    IncompatibleTask,
}

/// What a report to `HarnessOptions::on_report` is about. `extension`, where present, names the extension whose code
/// failed, when known; for a task's own code, the one that installed the definition the task ran.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReportInfo {
    /// The error failed the Session, which closes: a Storage error, often a platform reset.
    SessionFailed,
    TaskFaulted {
        task: ReportTask,
        #[serde(skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
        reason: FaultReason,
    },
    /// Crashes ended a built-in task, with its restart count when it ended.
    TaskInterrupted {
        task: ReportTask,
        #[serde(skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
        reason: InterruptionReason,
        restarts: u32,
    },
    /// The task's stored state could not be migrated; it is blocked until the registry changes.
    MigrationFailed {
        task: ReportTask,
        #[serde(skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
    },
    TaskOutdated {
        task: ReportTask,
        #[serde(skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
        reason: OutdatedReason,
    },
    /// The task's own `runtime.report()`, or a failure in running it.
    TaskError {
        task: ReportTask,
        #[serde(skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
    },
    /// A hook, a section, a wrapper, or a tool's invalid structured output; with the task that ran it, or, outside a
    /// task, the conversation whose agent was resolved.
    ExtensionError {
        #[serde(skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        task: Option<ReportTask>,
        #[serde(rename = "conversationId", skip_serializing_if = "Option::is_none")]
        conversation_id: Option<ConversationId>,
    },
    /// A host callback failed; with the task that called it, if one did.
    HostError {
        callback: HostCallback,
        #[serde(skip_serializing_if = "Option::is_none")]
        task: Option<ReportTask>,
    },
    /// A commit, close, watch, view, or document listener failed.
    ListenerError,
}

/// Whose code a task's `runtime.report_from()` is about when not the task's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReportFrom {
    /// An extension's, such as a section.
    Extension(Option<String>),
    /// A host callback, such as the environment builder.
    Host(HostCallback),
}
