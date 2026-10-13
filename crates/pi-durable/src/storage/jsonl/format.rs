//! Line formats of `main.jsonl` commit markers and `doc-<id>.jsonl` / `task-<id>.jsonl` sidecar records.

use pi_chord::{CopyJsonOptions, Json, JsonMap, copy_json};
use serde::Serialize;

use crate::errors::{DurableError, DurableResult};
use crate::ids::{DocumentId, Seq, TaskId};
use crate::storage::scan::MAX_SAFE_INTEGER;
use crate::types::{DocumentContent, DocumentCreate, StorageWrite, TaskRecord};

pub(crate) const FORMAT_VERSION: u64 = 1;
pub(crate) const MAIN_FILE: &str = "main.jsonl";
pub(crate) const RECLAIM_SUFFIX: &str = ".reclaim";

/// One write of a commit marker; sidecar-backed writes name their record by ordinal.
#[derive(Clone, Debug)]
pub(crate) enum MainOperation {
    /// Conversation, entry, submission, terminal task, and document retirement writes, stored whole.
    Inline(StorageWrite),
    TaskSidecar {
        id: TaskId,
        ordinal: u64,
    },
    DocumentCreate {
        record: DocumentCreate,
        ordinal: u64,
    },
    DocumentChange {
        id: DocumentId,
        ordinal: u64,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct MainMarker {
    pub seq: Seq,
    pub writes: Vec<MainOperation>,
}

#[derive(Clone, Debug)]
pub(crate) enum SidecarPayload {
    Task(Box<TaskRecord>),
    Document { id: DocumentId, content: DocumentContent },
}

/// One parsed sidecar line and the JSON it was parsed from, which reclamation writes back.
#[derive(Clone, Debug)]
pub(crate) struct SidecarRecord {
    pub seq: Seq,
    pub ordinal: u64,
    pub payload: SidecarPayload,
    pub json: Json,
}

/// A failure to open a JSONL directory whose confirmed data is unusable (upstream `JsonlCorruptionError`).
pub(crate) fn corruption(message: impl Into<String>) -> DurableError {
    DurableError::storage(message.into())
}

pub(crate) fn sidecar_file_name(kind: &str, id: u64) -> String {
    format!("{kind}-{id}.jsonl")
}

fn numeric_name(name: &str, suffix: &str) -> Option<u64> {
    let rest = name.strip_prefix("doc-").or_else(|| name.strip_prefix("task-"))?;
    let digits = rest.strip_suffix(suffix)?;
    let canonical = digits == "0" || (!digits.is_empty() && !digits.starts_with('0'));
    if !canonical || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The ID of a `doc-<id>.jsonl` or `task-<id>.jsonl` file name.
pub(crate) fn sidecar_id(name: &str) -> Option<u64> {
    numeric_name(name, ".jsonl")
}

pub(crate) fn is_reclaim_file_name(name: &str) -> bool {
    numeric_name(name, ".jsonl.reclaim").is_some()
}

pub(crate) fn to_json<T: Serialize + ?Sized>(value: &T) -> DurableResult<Json> {
    Ok(copy_json(value, CopyJsonOptions { omit_undefined_properties: true })?)
}

pub(crate) fn line(value: &Json) -> String {
    format!("{value}\n")
}

/// Content of a create resolved from a `document.copy`, in the key order upstream builds it.
pub(crate) fn copied_content_json(content: &DocumentContent) -> DurableResult<Json> {
    let DocumentContent::Base { version, value } = content else { return to_json(content) };
    let mut map = JsonMap::new();
    map.insert("kind", "base");
    map.insert("version", u64::from(*version));
    map.insert("value", value.clone());
    Ok(Json::object(map))
}

pub(crate) fn main_operation_json(operation: &MainOperation) -> DurableResult<Json> {
    let mut map = JsonMap::new();
    match operation {
        MainOperation::Inline(write) => return to_json(write),
        MainOperation::TaskSidecar { id, ordinal } => {
            map.insert("type", "task.sidecar");
            map.insert("id", id.0);
            map.insert("ordinal", *ordinal);
        }
        MainOperation::DocumentCreate { record, ordinal } => {
            map.insert("type", "document.create");
            map.insert("record", to_json(record)?);
            map.insert("ordinal", *ordinal);
        }
        MainOperation::DocumentChange { id, ordinal } => {
            map.insert("type", "document.change");
            map.insert("id", id.0);
            map.insert("ordinal", *ordinal);
        }
    }
    Ok(Json::object(map))
}

pub(crate) fn marker_json(seq: Seq, writes: Vec<Json>) -> Json {
    let mut map = JsonMap::new();
    map.insert("format", FORMAT_VERSION);
    map.insert("type", "commit");
    map.insert("seq", seq.0);
    map.insert("writes", Json::array(writes));
    Json::object(map)
}

pub(crate) fn record_json(seq: Seq, ordinal: u64, payload: Json) -> Json {
    let mut map = JsonMap::new();
    map.insert("format", FORMAT_VERSION);
    map.insert("type", "record");
    map.insert("seq", seq.0);
    map.insert("ordinal", ordinal);
    map.insert("payload", payload);
    Json::object(map)
}

fn safe_integer(value: Option<&Json>) -> Option<u64> {
    let number = value?.as_f64()?;
    (number.fract() == 0.0 && (0.0..=MAX_SAFE_INTEGER as f64).contains(&number)).then_some(number as u64)
}

fn is_object(value: Option<&Json>) -> bool {
    matches!(value, Some(Json::Object(_)))
}

fn parse_json(text: &str, description: &str) -> DurableResult<Json> {
    Json::parse(text).map_err(|error| corruption(format!("Malformed complete {description}: {error}")))
}

fn typed<T: serde::de::DeserializeOwned>(value: &Json, message: impl Fn() -> String) -> DurableResult<T> {
    value.deserialize_into().map_err(|error| corruption(format!("{}: {error}", message())))
}

fn parse_main_operation(value: &Json, description: &str) -> DurableResult<MainOperation> {
    let invalid = |what: &str| corruption(format!("Invalid {what} in {description}"));
    let Some(kind) = value.get("type").and_then(Json::as_str) else { return Err(invalid("write")) };
    let ordinal = || safe_integer(value.get("ordinal"));
    match kind {
        "conversation" | "entry" | "submission" => {
            if !is_object(value.get("value")) || safe_integer(value["value"].get("id")).is_none() {
                return Err(invalid(&format!("{kind} write")));
            }
            Ok(MainOperation::Inline(typed(value, || format!("Invalid {kind} write in {description}"))?))
        }
        "task" => {
            let task = &value["value"];
            let terminal = task.get("state").and_then(|state| state.get("status")).and_then(Json::as_str);
            if !is_object(value.get("value")) || safe_integer(task.get("id")).is_none() || terminal != Some("terminal")
            {
                return Err(invalid("terminal task write"));
            }
            Ok(MainOperation::Inline(typed(value, || format!("Invalid terminal task write in {description}"))?))
        }
        "document.retire" => match safe_integer(value.get("id")) {
            Some(id) => Ok(MainOperation::Inline(StorageWrite::DocumentRetire { id: DocumentId(id) })),
            None => Err(invalid("document retirement")),
        },
        "task.sidecar" => match (safe_integer(value.get("id")), ordinal()) {
            (Some(id), Some(ordinal)) => Ok(MainOperation::TaskSidecar { id: TaskId(id), ordinal }),
            _ => Err(invalid("task sidecar write")),
        },
        "document.create" => {
            let record = value.get("record");
            match (is_object(record), record.and_then(|record| safe_integer(record.get("id"))), ordinal()) {
                (true, Some(_), Some(ordinal)) => {
                    let record = typed(&value["record"], || format!("Invalid document creation in {description}"))?;
                    Ok(MainOperation::DocumentCreate { record, ordinal })
                }
                _ => Err(invalid("document creation")),
            }
        }
        "document.change" => match (safe_integer(value.get("id")), ordinal()) {
            (Some(id), Some(ordinal)) => Ok(MainOperation::DocumentChange { id: DocumentId(id), ordinal }),
            _ => Err(invalid("document change")),
        },
        _ => Err(corruption(format!("Unknown write type in {description}"))),
    }
}

pub(crate) fn parse_main_marker(text: &str, line_number: usize) -> DurableResult<MainMarker> {
    let description = format!("{MAIN_FILE} line {line_number}");
    let value = parse_json(text, &description)?;
    let seq = safe_integer(value.get("seq")).filter(|seq| *seq >= 1);
    let valid = is_object(Some(&value))
        && safe_integer(value.get("format")) == Some(FORMAT_VERSION)
        && value.get("type").and_then(Json::as_str) == Some("commit");
    let (Some(seq), Some(writes), true) = (seq, value.get("writes").and_then(Json::as_array), valid) else {
        return Err(corruption(format!("Invalid commit marker in {description}")));
    };
    let writes = writes.iter().map(|write| parse_main_operation(write, &description)).collect::<DurableResult<_>>()?;
    Ok(MainMarker { seq: Seq(seq), writes })
}

fn validate_document_content(value: &Json, description: &str) -> DurableResult<DocumentContent> {
    let invalid = || corruption(format!("Invalid document content in {description}"));
    if !is_object(Some(value)) || safe_integer(value.get("version")).is_none_or(|version| version < 1) {
        return Err(invalid());
    }
    let kind = value.get("kind").and_then(Json::as_str);
    let shaped = match kind {
        Some("base") => is_object(value.get("value")),
        Some("delta") => value.get("ops").and_then(Json::as_array).is_some(),
        _ => false,
    };
    if !shaped {
        return Err(invalid());
    }
    typed(value, || format!("Invalid document content in {description}"))
}

pub(crate) fn parse_sidecar_record(text: &str, file: &str, line_number: usize) -> DurableResult<SidecarRecord> {
    let description = format!("{file} line {line_number}");
    let value = parse_json(text, &description)?;
    let seq = safe_integer(value.get("seq")).filter(|seq| *seq >= 1);
    let ordinal = safe_integer(value.get("ordinal"));
    let payload = &value["payload"];
    let payload_type = payload.get("type").and_then(Json::as_str);
    let valid = is_object(Some(&value))
        && safe_integer(value.get("format")) == Some(FORMAT_VERSION)
        && value.get("type").and_then(Json::as_str) == Some("record")
        && is_object(value.get("payload"));
    let (Some(seq), Some(ordinal), Some(payload_type), true) = (seq, ordinal, payload_type, valid) else {
        return Err(corruption(format!("Invalid sidecar record in {description}")));
    };
    let payload = match payload_type {
        "task" => {
            let task = &payload["value"];
            let status = task.get("state").and_then(|state| state.get("status"));
            let live = is_object(task.get("state")) && status.and_then(Json::as_str) != Some("terminal");
            if !is_object(payload.get("value")) || safe_integer(task.get("id")).is_none() || !live {
                return Err(corruption(format!("Invalid live task record in {description}")));
            }
            SidecarPayload::Task(Box::new(typed(task, || format!("Invalid live task record in {description}"))?))
        }
        "document" => {
            let Some(id) = safe_integer(payload.get("id")) else {
                return Err(corruption(format!("Invalid document record in {description}")));
            };
            let content = validate_document_content(&payload["content"], &description)?;
            SidecarPayload::Document { id: DocumentId(id), content }
        }
        _ => return Err(corruption(format!("Unknown sidecar record type in {description}"))),
    };
    Ok(SidecarRecord { seq: Seq(seq), ordinal, payload, json: value })
}
