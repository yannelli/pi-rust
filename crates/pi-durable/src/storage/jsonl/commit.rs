//! The JSONL publication protocol: sidecar records, then one main marker, then in-memory adoption and reclamation.

use std::collections::{HashMap, HashSet};

use pi_chord::{Context, Json, JsonMap};

use super::format::{
    MAIN_FILE, MainOperation, RECLAIM_SUFFIX, copied_content_json, line, main_operation_json, marker_json, record_json,
    sidecar_file_name, to_json,
};
use super::{JsonlStorage, error_from_file};
use crate::env::RemoveOptions;
use crate::errors::DurableResult;
use crate::ids::{DocumentId, Seq, TaskId};
use crate::storage::is_current_only;
use crate::types::{DocumentContent, DocumentCreate, StorageWrite, TaskRecord};

/// The marker line and each affected sidecar's appended text, in first-use order.
pub(crate) struct EncodedCommit {
    pub marker: String,
    pub sidecars: Vec<(String, String)>,
}

impl EncodedCommit {
    fn sidecar(&self, file: &str) -> Option<&str> {
        self.sidecars.iter().find(|(name, _)| name == file).map(|(_, content)| content.as_str())
    }
}

pub(crate) fn create_is_current_only(record: &DocumentCreate) -> bool {
    is_current_only(&record.clone().into_record(Seq(0)))
}

fn document_payload(id: DocumentId, content: Json) -> Json {
    let mut map = JsonMap::new();
    map.insert("type", "document");
    map.insert("id", id.0);
    map.insert("content", content);
    Json::object(map)
}

fn task_payload(task: &TaskRecord) -> DurableResult<Json> {
    let mut map = JsonMap::new();
    map.insert("type", "task");
    map.insert("value", to_json(task)?);
    Ok(Json::object(map))
}

/// Encode a prepared commit. `original` is the batch as given, which tells which creates resolved a copy.
pub(crate) fn encode_commit(
    seq: Seq,
    original: &[StorageWrite],
    writes: &[StorageWrite],
) -> DurableResult<EncodedCommit> {
    let mut main = Vec::new();
    let mut records: Vec<(String, Vec<Json>)> = Vec::new();
    let mut next_ordinal = 0_u64;
    let mut add = |file: String, payload: Json| {
        let ordinal = next_ordinal;
        next_ordinal += 1;
        let record = record_json(seq, ordinal, payload);
        match records.iter_mut().find(|(name, _)| *name == file) {
            Some((_, lines)) => lines.push(record),
            None => records.push((file, vec![record])),
        }
        ordinal
    };
    for (index, write) in writes.iter().enumerate() {
        let operation = match write {
            StorageWrite::Task { value } if !value.is_terminal() => {
                let ordinal = add(sidecar_file_name("task", value.id.0), task_payload(value)?);
                MainOperation::TaskSidecar { id: value.id, ordinal }
            }
            StorageWrite::DocumentCreate { record, content } => {
                let copied = matches!(original.get(index), Some(StorageWrite::DocumentCopy { .. }));
                let content = if copied { copied_content_json(content)? } else { to_json(content)? };
                let ordinal = add(sidecar_file_name("doc", record.id.0), document_payload(record.id, content));
                MainOperation::DocumentCreate { record: record.clone(), ordinal }
            }
            StorageWrite::DocumentChange { id, content } => {
                let ordinal = add(sidecar_file_name("doc", id.0), document_payload(*id, to_json(content)?));
                MainOperation::DocumentChange { id: *id, ordinal }
            }
            other => MainOperation::Inline(other.clone()),
        };
        main.push(main_operation_json(&operation)?);
    }
    let sidecars = records.into_iter().map(|(file, lines)| (file, lines.iter().map(line).collect())).collect();
    Ok(EncodedCommit { marker: line(&marker_json(seq, main)), sidecars })
}

impl JsonlStorage {
    /// Sidecar files a committed batch makes reclaimable, with their replacement text ("" removes the file).
    fn plan_reclamations(&self, writes: &[StorageWrite], encoded: &EncodedCommit) -> Vec<(String, String)> {
        let mut created_current_only = HashSet::new();
        let mut retired = Vec::new();
        let mut bases = Vec::new();
        let mut final_tasks: Vec<(TaskId, bool)> = Vec::new();
        for write in writes {
            match write {
                StorageWrite::DocumentCreate { record, .. } if create_is_current_only(record) => {
                    created_current_only.insert(record.id);
                }
                StorageWrite::DocumentChange { id, content: DocumentContent::Base { .. } } => bases.push(*id),
                StorageWrite::DocumentRetire { id } => retired.push(*id),
                StorageWrite::Task { value } => match final_tasks.iter_mut().find(|(id, _)| *id == value.id) {
                    Some(entry) => entry.1 = value.is_terminal(),
                    None => final_tasks.push((value.id, value.is_terminal())),
                },
                _ => {}
            }
        }
        let known = self.current_only_documents.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let current_only = |id: &DocumentId| known.contains(id) || created_current_only.contains(id);
        let mut replacements: Vec<(String, String)> = Vec::new();
        let mut set = |file: String, content: String| match replacements.iter_mut().find(|(name, _)| *name == file) {
            Some(entry) => entry.1 = content,
            None => replacements.push((file, content)),
        };
        for id in retired.iter().filter(|id| current_only(id)) {
            set(sidecar_file_name("doc", id.0), String::new());
        }
        for id in &bases {
            if !current_only(id) || retired.contains(id) {
                continue;
            }
            let file = sidecar_file_name("doc", id.0);
            if let Some(content) = encoded.sidecar(&file) {
                let content = content.to_string();
                set(file, content);
            }
        }
        let live = self.live_task_sidecars.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for (id, terminal) in final_tasks {
            let file = sidecar_file_name("task", id.0);
            if terminal && (live.contains(&id) || encoded.sidecar(&file).is_some()) {
                set(file, String::new());
            }
        }
        replacements
    }

    fn adopt_sidecar_state(&self, writes: &[StorageWrite]) {
        let mut documents = self.current_only_documents.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut tasks = self.live_task_sidecars.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for write in writes {
            match write {
                StorageWrite::DocumentCreate { record, .. } if create_is_current_only(record) => {
                    documents.insert(record.id);
                }
                StorageWrite::Task { value } if value.is_terminal() => {
                    tasks.remove(&value.id);
                }
                StorageWrite::Task { value } => {
                    tasks.insert(value.id);
                }
                _ => {}
            }
        }
    }

    /// The marker already published this state, so reclamation is retryable best-effort maintenance.
    pub(crate) async fn reclaim_sidecars(&self, replacements: &[(String, String)], context: &Context) {
        if replacements.is_empty() {
            return;
        }
        if self.fsync && self.fs.flush_file(&self.main_path, context).await.is_err() {
            return;
        }
        for (file, content) in replacements {
            self.replace_sidecar(file, content, context).await;
        }
    }

    async fn replace_sidecar(&self, file: &str, content: &str, context: &Context) {
        let Ok(path) = self.fs.join_path(&[self.directory.clone(), file.to_string()], context).await else { return };
        if content.is_empty() {
            let _ = self.fs.remove(&path, RemoveOptions { recursive: false, force: true }, context).await;
            return;
        }
        let temporary = [self.directory.clone(), format!("{file}{RECLAIM_SUFFIX}")];
        let Ok(temporary) = self.fs.join_path(&temporary, context).await else { return };
        if self.fs.write_file(&temporary, content.as_bytes(), context).await.is_err() {
            return;
        }
        if self.fsync && self.fs.flush_file(&temporary, context).await.is_err() {
            return;
        }
        let _ = self.fs.rename_file(&temporary, &path, context).await;
    }

    pub(crate) async fn commit_writes(&self, writes: &[StorageWrite], context: &Context) -> DurableResult<Seq> {
        let _line = self.commit_line.lock().await;
        self.assert_usable()?;
        let prepared = self.memory.prepare_commit(writes, None)?;
        let encoded = encode_commit(prepared.seq(), writes, prepared.writes())?;
        let reclamations = self.plan_reclamations(prepared.writes(), &encoded);
        let mut paths = HashMap::new();
        for (file, _) in &encoded.sidecars {
            paths.insert(file.clone(), self.resolve_file(file, context).await?);
        }
        for (file, content) in &encoded.sidecars {
            if let Err(error) = self.fs.append_file(&paths[file], content.as_bytes(), context).await {
                return Err(self.poison(error_from_file(&format!("append to {file}"), error)));
            }
        }
        if self.fsync {
            for (file, _) in &encoded.sidecars {
                if let Err(error) = self.fs.flush_file(&paths[file], context).await {
                    return Err(self.poison(error_from_file(&format!("flush of {file}"), error)));
                }
            }
        }
        if let Err(error) = self.fs.append_file(&self.main_path, encoded.marker.as_bytes(), context).await {
            return Err(self.poison(error_from_file(&format!("append to {MAIN_FILE}"), error)));
        }
        let seq = self.memory.apply(&prepared);
        self.adopt_sidecar_state(prepared.writes());
        self.reclaim_sidecars(&reclamations, context).await;
        Ok(seq)
    }
}
