//! `/bug` report contents: redacted metadata, failed-turn diagnostics, the optional transcript
//! and summary, and the zip export (upstream `core/bug-report.ts`).

use std::collections::BTreeMap;
use std::path::Path;

use pi_ai::{AssistantMessage, Message, Model, StopReason};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::crash_log::CrashRecord;
use super::redact::{redact_json_value, redact_settings, redact_url};
use super::zip::{ZipEntry, write_zip_archive};
use crate::platform::http::{VERSION, pi_user_agent};
use crate::session::Entry;

pub const BUG_REPORT_CUSTOM_ENTRY_TYPE: &str = "pi.bug-report";
const BUG_REPORT_SCHEMA_VERSION: u32 = 1;

pub const BUG_REPORT_DISCLAIMER: &str = "This report goes to the Pi developers (Earendil) and is not shared publicly. It includes your pi version, operating system, the current model and provider configuration (without API keys), loaded extensions, settings, and provider error diagnostics from this session.";
pub const BUG_REPORT_TRANSCRIPT_NOTE: &str = "The transcript contains your messages, model output, tool calls and their results, including file contents and command output read during this session.";

fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

fn read_trimmed(path: &str) -> String {
    std::fs::read_to_string(path).map(|text| text.trim().to_string()).unwrap_or_default()
}

/// Version, platform, terminal, and the names (never values) of `PI_*` variables.
pub fn collect_environment(vars: &BTreeMap<String, String>) -> Value {
    let env = |name: &str| vars.get(name).filter(|value| !value.is_empty()).cloned();
    let present = |name: &str| env(name).is_some();
    let shell =
        env("SHELL").and_then(|shell| shell.rsplit(['/', '\\']).next().map(String::from)).filter(|s| !s.is_empty());
    let names: Vec<&String> = vars.keys().filter(|name| name.starts_with("PI_")).collect();
    json!({
        "version": VERSION,
        "userAgent": pi_user_agent(VERSION),
        "runtime": "rust",
        "platform": node_platform(),
        "arch": node_arch(),
        "osRelease": read_trimmed("/proc/sys/kernel/osrelease"),
        "osVersion": read_trimmed("/proc/sys/kernel/version"),
        "shell": shell,
        "terminal": {
            "term": env("TERM"),
            "program": env("TERM_PROGRAM"),
            "programVersion": env("TERM_PROGRAM_VERSION"),
            "colorterm": env("COLORTERM"),
            "tmux": present("TMUX"),
            "ssh": present("SSH_CONNECTION") || present("SSH_CLIENT") || present("SSH_TTY"),
            "ci": present("CI"),
        },
        "piEnvironmentVariables": names,
    })
}

pub fn describe_model(model: &Model) -> Value {
    let mut header_names: Vec<&String> = model.headers.keys().collect();
    header_names.sort();
    json!({
        "provider": model.provider,
        "id": model.id,
        "name": model.name,
        "api": model.api,
        "baseUrl": redact_url(&model.base_url),
        "reasoning": model.reasoning,
        "input": model.input,
        "contextWindow": model.context_window,
        "maxTokens": model.max_tokens,
        "samplingParams": model.sampling_params.as_ref().map(|params| redact_json_value(&Value::Object(params.clone()))),
        "compat": model.compat.as_ref().map(redact_json_value),
        "thinkingLevelMap": model.thinking_level_map,
        "headerNames": header_names,
    })
}

/// What the report says about the model's provider; the app fills it from its model runtime.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderDescription {
    pub id: String,
    pub name: String,
    pub base_url: Option<String>,
    pub header_names: Vec<String>,
    /// `api_key` and `oauth` methods the provider supports.
    pub auth_types: Vec<String>,
    pub auth_status: Value,
    pub using_oauth: bool,
    pub registered_by_extension: bool,
}

pub fn describe_provider(provider: &ProviderDescription) -> Value {
    let mut header_names = provider.header_names.clone();
    header_names.sort();
    json!({
        "id": provider.id,
        "name": provider.name,
        "baseUrl": provider.base_url.as_deref().map(redact_url),
        "headerNames": header_names,
        "authTypes": provider.auth_types,
        "authStatus": provider.auth_status,
        "usingOAuth": provider.using_oauth,
        "registeredByExtension": provider.registered_by_extension,
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtensionDescription {
    pub path: String,
    pub source: String,
    pub scope: String,
    pub origin: String,
    pub hidden: bool,
}

#[derive(Clone, Debug, Default)]
pub struct BugReportMetadataOptions {
    /// A UUID v7 when not given.
    pub id: Option<String>,
    pub hint: Option<String>,
    pub session_id: String,
    pub cwd: String,
    pub include_session: bool,
    pub include_summary: bool,
    pub message_count: usize,
    pub model: Option<Model>,
    pub provider: Option<ProviderDescription>,
    pub thinking_level: String,
    pub extensions: Vec<ExtensionDescription>,
    /// `(path, error)` of extensions that failed to load.
    pub extension_errors: Vec<(String, String)>,
    pub global_settings: Value,
    pub project_settings: Value,
    /// The process environment; `std::env::vars()` when empty.
    pub environment: BTreeMap<String, String>,
}

/// `report.json`: what the user said, where pi runs, and the redacted configuration.
pub fn collect_bug_report_metadata(options: &BugReportMetadataOptions) -> Value {
    let hint = options.hint.as_deref().map(str::trim).filter(|hint| !hint.is_empty());
    let mut session = json!({
        "id": options.session_id,
        "included": options.include_session,
        "summaryIncluded": options.include_summary,
        "messageCount": options.message_count,
    });
    if options.include_session {
        session["cwd"] = json!(options.cwd);
    }
    let environment = if options.environment.is_empty() {
        collect_environment(&std::env::vars().collect())
    } else {
        collect_environment(&options.environment)
    };
    let extensions: Vec<Value> = options
        .extensions
        .iter()
        .map(|extension| {
            json!({"path": extension.path, "source": redact_url(&extension.source), "scope": extension.scope,
                "origin": extension.origin, "hidden": extension.hidden})
        })
        .collect();
    let errors: Vec<Value> =
        options.extension_errors.iter().map(|(path, error)| json!({"path": path, "error": error})).collect();
    json!({
        "schemaVersion": BUG_REPORT_SCHEMA_VERSION,
        "id": options.id.clone().unwrap_or_else(crate::session::uuid_v7),
        "createdAt": crate::session::iso_timestamp(),
        "hint": hint,
        "environment": environment,
        "session": session,
        "model": options.model.as_ref().map(describe_model),
        "provider": options.provider.as_ref().map(describe_provider),
        "thinkingLevel": options.thinking_level,
        "extensions": extensions,
        "extensionErrors": errors,
        "settings": {
            "global": redact_settings(&options.global_settings),
            "project": redact_settings(&options.project_settings),
        },
    })
}

fn failed_turn(entry: &Entry, message: &AssistantMessage) -> Option<Value> {
    let failed = matches!(message.stop_reason, StopReason::Error | StopReason::Aborted);
    if message.diagnostics.is_empty() && !failed && message.error_message.is_none() {
        return None;
    }
    let mut turn = json!({
        "entryId": entry.id(),
        "timestamp": entry.timestamp(),
        "provider": message.provider,
        "model": message.model,
        "api": message.api,
        "stopReason": message.stop_reason,
    });
    if let Some(raw) = &message.raw_stop_reason {
        turn["rawStopReason"] = json!(raw);
    }
    if let Some(error) = &message.error_message {
        turn["errorMessage"] = json!(error);
    }
    turn["diagnostics"] = json!(message.diagnostics);
    Some(turn)
}

/// `diagnostics.json`: failed assistant turns and recent crashes, without conversation content.
pub fn collect_bug_report_diagnostics(session_id: &str, entries: &[Entry], crashes: &[CrashRecord]) -> Value {
    let mut assistant_count = 0;
    let mut assistant = Vec::new();
    for entry in entries {
        let Some(Message::Assistant(message)) = entry.message() else { continue };
        assistant_count += 1;
        assistant.extend(failed_turn(entry, message));
    }
    let crashes: Vec<CrashRecord> =
        crashes.iter().map(|crash| CrashRecord { notified: None, ..crash.clone() }).collect();
    json!({
        "schemaVersion": BUG_REPORT_SCHEMA_VERSION,
        "sessionId": session_id,
        "entryCount": entries.len(),
        "assistantMessageCount": assistant_count,
        "assistant": assistant,
        "crashes": crashes,
    })
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BugReportBundle {
    pub metadata: Value,
    pub diagnostics: Value,
    pub session_jsonl: Option<String>,
    pub summary: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BugReportFile {
    pub name: &'static str,
    pub content_type: &'static str,
    pub data: String,
}

fn pretty(value: &Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(value).unwrap_or_default())
}

/// Files shared by the upload and the zip export.
pub fn bug_report_files(bundle: &BugReportBundle) -> Vec<BugReportFile> {
    let mut files = vec![
        BugReportFile { name: "report.json", content_type: "application/json", data: pretty(&bundle.metadata) },
        BugReportFile { name: "diagnostics.json", content_type: "application/json", data: pretty(&bundle.diagnostics) },
    ];
    if let Some(session) = &bundle.session_jsonl {
        files.push(BugReportFile {
            name: "session.jsonl",
            content_type: "application/x-ndjson",
            data: session.clone(),
        });
    }
    if let Some(summary) = &bundle.summary {
        let data = if summary.ends_with('\n') { summary.clone() } else { format!("{summary}\n") };
        files.push(BugReportFile { name: "summary.md", content_type: "text/markdown", data });
    }
    files
}

pub fn write_bug_report_archive(bundle: &BugReportBundle, path: &Path) -> std::io::Result<()> {
    let entries: Vec<ZipEntry> =
        bug_report_files(bundle).into_iter().map(|file| ZipEntry::new(file.name, file.data.into_bytes())).collect();
    write_zip_archive(path, &entries)
}

pub fn bug_report_archive_file_name(id: &str) -> String {
    format!("pi-bug-report-{id}.zip")
}

/// The `pi.bug-report` custom entry recorded in the session after delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BugReportSessionEntryData {
    pub id: String,
    pub created_at: String,
    pub hint: Option<String>,
    pub session_included: bool,
    pub summary_included: bool,
    /// `zip` or `upload`.
    pub delivery: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl BugReportSessionEntryData {
    pub fn from_bundle(bundle: &BugReportBundle, delivery: &str, path: Option<String>) -> Self {
        let text = |key: &str| bundle.metadata.get(key).and_then(Value::as_str).map(String::from);
        let session = |key: &str| bundle.metadata["session"].get(key).and_then(Value::as_bool).unwrap_or(false);
        Self {
            id: text("id").unwrap_or_default(),
            created_at: text("createdAt").unwrap_or_default(),
            hint: text("hint"),
            session_included: session("included"),
            summary_included: session("summaryIncluded"),
            delivery: delivery.into(),
            path,
        }
    }
}
