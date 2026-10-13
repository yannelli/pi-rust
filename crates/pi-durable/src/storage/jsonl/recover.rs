//! Recovery on open: drop torn and unconfirmed tails, replay confirmed records, and reclaim what later commits
//! made unnecessary.

use std::collections::{HashMap, HashSet};

use pi_chord::{Context, Json};

use super::commit::create_is_current_only;
use super::format::{
    MAIN_FILE, MainMarker, MainOperation, SidecarPayload, SidecarRecord, corruption, is_reclaim_file_name, line,
    parse_main_marker, parse_sidecar_record, sidecar_file_name, sidecar_id,
};
use super::{JsonlStorage, error_from_file};
use crate::env::{FileErrorCode, FileKind, FileSystem, RemoveOptions};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{DocumentId, Seq, TaskId};
use crate::types::{DocumentContent, StorageWrite};

type Key = (String, u64, u64);

struct ParsedLine<T> {
    value: T,
    start: u64,
}

struct ParsedFile<T> {
    path: String,
    lines: Vec<ParsedLine<T>>,
}

/// Read the complete lines of one file, truncating a torn final line first.
async fn read_lines<T>(
    fs: &dyn FileSystem,
    path: &str,
    name: &str,
    context: &Context,
    parse: impl Fn(&str, usize) -> DurableResult<T>,
) -> DurableResult<ParsedFile<T>> {
    let bytes = match fs.read_binary_file(path, context).await {
        Ok(bytes) => bytes,
        Err(error) if error.code == FileErrorCode::NotFound => {
            return Ok(ParsedFile { path: path.to_string(), lines: Vec::new() });
        }
        Err(error) => return Err(error_from_file(&format!("read of {name}"), error)),
    };
    let mut complete = bytes.len();
    if complete > 0 && bytes[complete - 1] != b'\n' {
        complete = bytes.iter().rposition(|byte| *byte == b'\n').map_or(0, |index| index + 1);
        let truncated = fs.truncate_file(path, complete as u64, context).await;
        truncated.map_err(|error| error_from_file(&format!("torn-line truncation of {name}"), error))?;
    }
    let mut lines = Vec::new();
    let mut start = 0;
    for (number, end) in (0..complete).filter(|index| bytes[*index] == b'\n').enumerate() {
        let text = std::str::from_utf8(&bytes[start..end])
            .map_err(|_| corruption(format!("Invalid UTF-8 in complete {name} line {}", number + 1)))?;
        lines.push(ParsedLine { value: parse(text, number + 1)?, start: start as u64 });
        start = end + 1;
    }
    Ok(ParsedFile { path: path.to_string(), lines })
}

fn key(file: &str, seq: Seq, ordinal: u64) -> Key {
    (file.to_string(), seq.0, ordinal)
}

fn is_before(base: Option<&(Seq, u64)>, seq: Seq, ordinal: u64) -> bool {
    base.is_some_and(|(base_seq, base_ordinal)| seq < *base_seq || (seq == *base_seq && ordinal < *base_ordinal))
}

/// Mark one sidecar record confirmed by a marker; a missing record is corruption unless `optional`.
fn confirm<'a>(
    marker: &MainMarker,
    ordinal: u64,
    file: &str,
    records: &'a HashMap<Key, SidecarRecord>,
    confirmed: &mut HashSet<Key>,
    optional: bool,
) -> DurableResult<Option<&'a SidecarRecord>> {
    let key = key(file, marker.seq, ordinal);
    if confirmed.contains(&key) {
        return Err(corruption("Sidecar record is confirmed more than once"));
    }
    match records.get(&key) {
        None if optional => Ok(None),
        None => Err(corruption(format!("Missing confirmed sidecar record {file} at sequence {}", marker.seq))),
        Some(record) => {
            confirmed.insert(key);
            Ok(Some(record))
        }
    }
}

impl JsonlStorage {
    pub(crate) async fn recover(&self, context: &Context) -> DurableResult<()> {
        let fs = &*self.fs;
        let main = read_lines(fs, &self.main_path, MAIN_FILE, context, parse_main_marker).await?;
        let mut last_seq = Seq(0);
        for marker in &main.lines {
            if marker.value.seq <= last_seq {
                return Err(corruption(format!("Commit sequence does not strictly increase in {MAIN_FILE}")));
            }
            last_seq = marker.value.seq;
        }

        let listed =
            fs.list_dir(&self.directory, context).await.map_err(|e| error_from_file("directory listing", e))?;
        for info in listed.iter().filter(|info| info.kind == FileKind::File && is_reclaim_file_name(&info.name)) {
            let _ = fs.remove(&info.path, RemoveOptions { recursive: false, force: true }, context).await;
        }
        let mut sidecar_files: Vec<String> = listed
            .iter()
            .filter(|info| info.kind == FileKind::File && sidecar_id(&info.name).is_some())
            .map(|info| info.name.clone())
            .collect();
        sidecar_files.sort();
        let mut parsed_files: Vec<(String, ParsedFile<SidecarRecord>)> = Vec::new();
        let mut records: HashMap<Key, SidecarRecord> = HashMap::new();
        for file in sidecar_files {
            let path = self.resolve_file(&file, context).await?;
            let parse = |text: &str, number: usize| parse_sidecar_record(text, &file, number);
            let parsed = read_lines(fs, &path, &file, context, parse).await?;
            let mut last: Option<(Seq, u64)> = None;
            for parsed_line in &parsed.lines {
                let record = &parsed_line.value;
                if last
                    .is_some_and(|(seq, ordinal)| record.seq < seq || (record.seq == seq && record.ordinal <= ordinal))
                {
                    return Err(corruption(format!("Sidecar records are out of order in {file}")));
                }
                last = Some((record.seq, record.ordinal));
                records.insert(key(&file, record.seq, record.ordinal), record.clone());
            }
            parsed_files.push((file, parsed));
        }

        let mut current_only = HashSet::new();
        let mut retired = HashSet::new();
        let mut final_task_is_live: Vec<(TaskId, bool)> = Vec::new();
        let mut set_live = |id: TaskId, live: bool| match final_task_is_live.iter_mut().find(|(task, _)| *task == id) {
            Some(entry) => entry.1 = live,
            None => final_task_is_live.push((id, live)),
        };
        for marker in &main.lines {
            for operation in &marker.value.writes {
                match operation {
                    MainOperation::DocumentCreate { record, .. } if create_is_current_only(record) => {
                        current_only.insert(record.id);
                    }
                    MainOperation::Inline(StorageWrite::DocumentRetire { id }) => {
                        retired.insert(*id);
                    }
                    MainOperation::Inline(StorageWrite::Task { value }) => set_live(value.id, false),
                    MainOperation::TaskSidecar { id, .. } => set_live(*id, true),
                    _ => {}
                }
            }
        }
        let retired_current_only: HashSet<DocumentId> = retired.intersection(&current_only).copied().collect();

        let mut latest_bases: HashMap<DocumentId, (Seq, u64)> = HashMap::new();
        for marker in &main.lines {
            for operation in &marker.value.writes {
                let (id, ordinal) = match operation {
                    MainOperation::DocumentCreate { record, ordinal } => (record.id, *ordinal),
                    MainOperation::DocumentChange { id, ordinal } => (*id, *ordinal),
                    _ => continue,
                };
                if !current_only.contains(&id) {
                    continue;
                }
                let found = records.get(&key(&sidecar_file_name("doc", id.0), marker.value.seq, ordinal));
                let Some(record) = found else { continue };
                let is_base = matches!(
                    &record.payload,
                    SidecarPayload::Document { id: payload_id, content: DocumentContent::Base { .. } } if *payload_id == id
                );
                let position = (record.seq, record.ordinal);
                if is_base && latest_bases.get(&id).is_none_or(|latest| position > *latest) {
                    latest_bases.insert(id, position);
                }
            }
        }

        let terminal_tasks: HashSet<TaskId> =
            final_task_is_live.iter().filter(|(_, live)| !live).map(|(id, _)| *id).collect();
        let mut confirmed: HashSet<Key> = HashSet::new();
        for parsed in &main.lines {
            let marker = &parsed.value;
            let mut writes = Vec::new();
            for operation in &marker.writes {
                match operation {
                    MainOperation::Inline(write) => writes.push(write.clone()),
                    MainOperation::TaskSidecar { id, ordinal } => {
                        let optional = terminal_tasks.contains(id);
                        let file = sidecar_file_name("task", id.0);
                        if let Some(record) = confirm(marker, *ordinal, &file, &records, &mut confirmed, optional)? {
                            let SidecarPayload::Task(task) = &record.payload else {
                                return Err(corruption(format!(
                                    "Confirmed task sidecar data does not match commit {}",
                                    marker.seq
                                )));
                            };
                            if task.id != *id {
                                return Err(corruption(format!(
                                    "Confirmed task sidecar data does not match commit {}",
                                    marker.seq
                                )));
                            }
                            if !optional {
                                writes.push(StorageWrite::Task { value: (**task).clone() });
                            }
                        }
                    }
                    MainOperation::DocumentCreate { record: create, ordinal } => {
                        let id = create.id;
                        let reclaimed = retired_current_only.contains(&id)
                            || is_before(latest_bases.get(&id), marker.seq, *ordinal);
                        let content =
                            self.confirmed_content(marker, id, *ordinal, &records, &mut confirmed, reclaimed)?;
                        if content.as_ref().is_some_and(|content| !matches!(content, DocumentContent::Base { .. })) {
                            return Err(corruption(format!(
                                "Document creation lacks a confirmed base in commit {}",
                                marker.seq
                            )));
                        }
                        let placeholder = DocumentContent::Base { version: 1, value: Json::empty_object() };
                        let content = if reclaimed { placeholder } else { content.unwrap_or(placeholder) };
                        writes.push(StorageWrite::DocumentCreate { record: create.clone(), content });
                    }
                    MainOperation::DocumentChange { id, ordinal } => {
                        let reclaimed =
                            retired_current_only.contains(id) || is_before(latest_bases.get(id), marker.seq, *ordinal);
                        let content =
                            self.confirmed_content(marker, *id, *ordinal, &records, &mut confirmed, reclaimed)?;
                        if let (false, Some(content)) = (reclaimed, content) {
                            writes.push(StorageWrite::DocumentChange { id: *id, content });
                        }
                    }
                }
            }
            let prepared = self
                .memory
                .prepare_commit(&writes, Some(marker.seq))
                .map_err(|error| corruption(format!("Invalid committed state at sequence {}: {error}", marker.seq)))?;
            self.memory.apply(&prepared);
        }

        let mut reclamations = Vec::new();
        for (file, parsed) in &parsed_files {
            let is_confirmed = |record: &SidecarRecord| confirmed.contains(&key(file, record.seq, record.ordinal));
            let mut unconfirmed_at = None;
            for parsed_line in &parsed.lines {
                if is_confirmed(&parsed_line.value) {
                    if unconfirmed_at.is_some() {
                        return Err(corruption(format!("Confirmed record follows an unconfirmed tail in {file}")));
                    }
                } else if unconfirmed_at.is_none() {
                    unconfirmed_at = Some(parsed_line.start);
                }
            }
            if let Some(at) = unconfirmed_at {
                let truncated = fs.truncate_file(&parsed.path, at, context).await;
                truncated.map_err(|error| error_from_file(&format!("tail truncation of {file}"), error))?;
            }
            let numeric_id = sidecar_id(file).expect("a listed sidecar name");
            let confirmed_lines: Vec<&SidecarRecord> = parsed
                .lines
                .iter()
                .map(|parsed_line| &parsed_line.value)
                .filter(|record| is_confirmed(record))
                .collect();
            let retained: Option<Vec<&SidecarRecord>> = if file.starts_with("task-") {
                terminal_tasks.contains(&TaskId(numeric_id)).then(Vec::new)
            } else {
                let document = DocumentId(numeric_id);
                if retired_current_only.contains(&document) {
                    Some(Vec::new())
                } else {
                    latest_bases.get(&document).map(|base| {
                        let base = Some(base);
                        confirmed_lines
                            .iter()
                            .copied()
                            .filter(|record| !is_before(base, record.seq, record.ordinal))
                            .collect()
                    })
                }
            };
            if let Some(retained) = retained
                && (retained.len() < confirmed_lines.len() || retained.is_empty())
            {
                reclamations.push((file.clone(), retained.iter().map(|record| line(&record.json)).collect()));
            }
        }
        self.reclaim_sidecars(&reclamations, context).await;

        self.current_only_documents.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).extend(current_only);
        let mut live = self.live_task_sidecars.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        live.extend(final_task_is_live.into_iter().filter(|(_, live)| *live).map(|(id, _)| id));
        Ok(())
    }

    fn confirmed_content(
        &self,
        marker: &MainMarker,
        id: DocumentId,
        ordinal: u64,
        records: &HashMap<Key, SidecarRecord>,
        confirmed: &mut HashSet<Key>,
        optional: bool,
    ) -> DurableResult<Option<DocumentContent>> {
        let file = sidecar_file_name("doc", id.0);
        let Some(record) = confirm(marker, ordinal, &file, records, confirmed, optional)? else { return Ok(None) };
        match &record.payload {
            SidecarPayload::Document { id: payload_id, content } if *payload_id == id => Ok(Some(content.clone())),
            _ => Err(DurableError::storage(format!(
                "Confirmed document sidecar data does not match commit {}",
                marker.seq
            ))),
        }
    }
}
