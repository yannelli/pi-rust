//! JSONL session files in the TypeScript pi v3 format: a `session` header line, then entries
//! linked by `parentId` into a tree. Port of upstream `core/session-manager.ts`; see
//! `docs/session-format.md`.

mod append;
pub mod branching;
pub mod context;
pub mod entry;
pub mod ids;
pub mod listing;
pub mod search;
pub mod storage;
pub mod tree;
pub mod usage;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

pub use self::append::NewCompaction;
pub use self::context::{
    Leaf, ProjectedEntry, SessionContext, SessionProjection, build_context_entries, build_session_context,
    build_session_path, build_session_projection, latest_compaction, session_entry_to_context_messages,
};
pub use self::entry::{
    BranchSummaryEntry, CURRENT_SESSION_VERSION, CompactionEntry, ContextReplacement, CustomMessageEntry, Entry,
    EntryData, FileEntry, SessionHeader, UsageEntry,
};
use self::ids::resolve_path;
pub use self::ids::{assert_valid_session_id, format_iso, iso_timestamp, parse_iso, uuid_v7};
pub use self::listing::{ResolvedSession, SessionInfo, list_sessions};
use self::storage::{append_line, header_from_raw, write_lines};
pub use self::storage::{
    find_most_recent_session, load_entries_from_file, migrate_session_entries, parse_session_entries,
    read_session_header,
};
pub use self::tree::{SessionTree, SessionTreeNode};
pub use crate::messages::{COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX};

/// The file format version written by this implementation.
pub const SESSION_VERSION: u32 = CURRENT_SESSION_VERSION;

/// Options for a new session: a custom id (validated) and the session it was forked from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewSessionOptions {
    pub id: Option<String>,
    pub parent_session: Option<String>,
}

/// An append-only entry tree with a movable leaf, persisted lazily to a JSONL file.
#[derive(Clone, Debug)]
pub struct SessionManager {
    session_id: String,
    session_file: Option<PathBuf>,
    session_dir: PathBuf,
    cwd: PathBuf,
    persist: bool,
    flushed: bool,
    header: FileEntry,
    entries: Vec<Entry>,
    by_id: HashMap<String, usize>,
    /// Target id to `(label, timestamp of the label change)`.
    labels: HashMap<String, (String, String)>,
    leaf: Option<String>,
}

/// `<agent>/sessions/--<cwd with separators replaced>--`, as in the TypeScript pi.
pub fn session_dir(agent_dir: &Path, cwd: &Path) -> PathBuf {
    let cwd = resolve_path(cwd).display().to_string();
    let trimmed = cwd.strip_prefix(['/', '\\']).unwrap_or(&cwd);
    resolve_path(agent_dir).join("sessions").join(format!("--{}--", trimmed.replace(['/', '\\', ':'], "-")))
}

impl SessionManager {
    fn construct(
        cwd: &Path,
        session_dir: PathBuf,
        session_file: Option<&Path>,
        persist: bool,
        options: NewSessionOptions,
        preloaded: Option<Vec<FileEntry>>,
    ) -> anyhow::Result<Self> {
        let mut manager = Self {
            session_id: String::new(),
            session_file: None,
            session_dir,
            cwd: resolve_path(cwd),
            persist,
            flushed: false,
            header: FileEntry::new(),
            entries: Vec::new(),
            by_id: HashMap::new(),
            labels: HashMap::new(),
            leaf: None,
        };
        if persist && !manager.session_dir.as_os_str().is_empty() {
            // Best effort: writes create missing directories again.
            let _ = std::fs::create_dir_all(&manager.session_dir);
        }
        match (session_file, preloaded) {
            (Some(file), preloaded) => manager.load_session_file(file, preloaded)?,
            (None, Some(entries)) if !entries.is_empty() => manager.load_entries(entries, options)?,
            (None, _) => {
                manager.new_session(options)?;
            }
        }
        Ok(manager)
    }

    /// A new session. `dir` is `None` for an in-memory session.
    pub fn create(cwd: &Path, dir: Option<&Path>) -> Self {
        Self::create_with(cwd, dir, NewSessionOptions::default()).unwrap_or_else(|_| Self::in_memory(cwd))
    }

    /// A new session with options; persisted in `dir`, or in memory for `None`.
    pub fn create_with(cwd: &Path, dir: Option<&Path>, options: NewSessionOptions) -> anyhow::Result<Self> {
        match dir {
            Some(dir) => Self::construct(cwd, resolve_path(dir), None, true, options, None),
            None => Self::in_memory_with(cwd, options, Vec::new()),
        }
    }

    /// An in-memory session that never touches the filesystem.
    pub fn in_memory(cwd: &Path) -> Self {
        Self::in_memory_with(cwd, NewSessionOptions::default(), Vec::new()).expect("default options are valid")
    }

    /// An in-memory session holding `entries` (with or without a header line).
    pub fn in_memory_with(cwd: &Path, options: NewSessionOptions, entries: Vec<FileEntry>) -> anyhow::Result<Self> {
        Self::construct(cwd, PathBuf::new(), None, false, options, Some(entries))
    }

    /// Open a session file; a missing file starts a new session at that path. The session
    /// directory is the file's parent and the cwd comes from the header.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with(path, None, None)
    }

    /// Open with an explicit session directory (for `/new` and forks) and cwd override.
    pub fn open_with(path: &Path, session_dir: Option<&Path>, cwd_override: Option<&Path>) -> anyhow::Result<Self> {
        let resolved = resolve_path(path);
        let mut preloaded = None;
        let mut header = None;
        if cwd_override.is_none() && resolved.exists() {
            match read_session_header(&resolved) {
                Ok(found) => header = found,
                Err(error) if error.is::<storage::HeaderScanLimit>() => {
                    let entries = load_entries_from_file(&resolved);
                    header = entries.first().and_then(header_from_raw);
                    preloaded = Some(entries);
                }
                Err(error) => return Err(error),
            }
        }
        let cwd = match (cwd_override, header.map(|header| header.cwd).filter(|cwd| !cwd.is_empty())) {
            (Some(cwd), _) => cwd.to_path_buf(),
            (None, Some(cwd)) => PathBuf::from(cwd),
            (None, None) => std::env::current_dir().unwrap_or_default(),
        };
        let dir = match session_dir {
            Some(dir) => resolve_path(dir),
            None => resolved.parent().map(Path::to_path_buf).unwrap_or_default(),
        };
        Self::construct(&cwd, dir, Some(&resolved), true, NewSessionOptions::default(), preloaded)
    }

    /// The most recent session in `dir` (for this cwd when `dir` is a shared flat directory),
    /// or a new one.
    pub fn continue_recent(cwd: &Path, dir: &Path, agent_dir: &Path) -> anyhow::Result<Self> {
        let dir = resolve_path(dir);
        let filter_cwd = dir != session_dir(agent_dir, cwd);
        match find_most_recent_session(&dir, filter_cwd.then_some(cwd)) {
            Some(path) => Self::construct(cwd, dir, Some(&path), true, NewSessionOptions::default(), None),
            None => Self::construct(cwd, dir, None, true, NewSessionOptions::default(), None),
        }
    }

    /// The session file in `dir` with the latest change time.
    pub fn most_recent(dir: &Path) -> Option<PathBuf> {
        find_most_recent_session(dir, None)
    }

    /// Switch to another session file (resume and branching).
    pub fn set_session_file(&mut self, path: &Path) -> anyhow::Result<()> {
        self.load_session_file(&resolve_path(path), None)
    }

    fn load_session_file(&mut self, path: &Path, preloaded: Option<Vec<FileEntry>>) -> anyhow::Result<()> {
        let path = resolve_path(path);
        if !path.exists() {
            self.new_session(NewSessionOptions::default())?;
            self.session_file = Some(path);
            return Ok(());
        }
        let entries = preloaded.unwrap_or_else(|| load_entries_from_file(&path));
        if entries.is_empty() {
            if std::fs::metadata(&path).map(|metadata| metadata.len()).unwrap_or(0) > 0 {
                anyhow::bail!("Session file is not a valid pi session: {}", path.display());
            }
            self.new_session(NewSessionOptions::default())?;
            self.session_file = Some(path);
            self.rewrite_file()?;
            self.flushed = true;
            return Ok(());
        }
        self.session_file = Some(path);
        self.load_entries(entries, NewSessionOptions::default())?;
        self.flushed = true;
        Ok(())
    }

    /// Start a fresh session in this manager. Returns the new file path when persisting.
    pub fn new_session(&mut self, options: NewSessionOptions) -> anyhow::Result<Option<PathBuf>> {
        if let Some(id) = &options.id {
            assert_valid_session_id(id)?;
        }
        self.session_id = options.id.unwrap_or_else(uuid_v7);
        let timestamp = iso_timestamp();
        let header = SessionHeader {
            kind: "session".into(),
            version: Some(CURRENT_SESSION_VERSION),
            id: self.session_id.clone(),
            timestamp: timestamp.clone(),
            cwd: self.cwd.display().to_string(),
            parent_session: options.parent_session,
        };
        self.header = match serde_json::to_value(&header)? {
            Value::Object(object) => object,
            _ => FileEntry::new(),
        };
        self.entries.clear();
        self.by_id.clear();
        self.labels.clear();
        self.leaf = None;
        self.flushed = false;
        if self.persist {
            let file_timestamp = timestamp.replace([':', '.'], "-");
            self.session_file = Some(self.session_dir.join(format!("{file_timestamp}_{}.jsonl", self.session_id)));
        }
        Ok(self.session_file.clone())
    }

    fn load_entries(&mut self, mut file_entries: Vec<FileEntry>, options: NewSessionOptions) -> anyhow::Result<()> {
        let header_index =
            file_entries.iter().position(|entry| entry.get("type").and_then(Value::as_str) == Some("session"));
        let Some(header_index) = header_index else {
            self.new_session(options)?;
            self.entries = file_entries.into_iter().map(Entry::from_raw).collect();
            self.build_index();
            return Ok(());
        };
        let migrated = migrate_session_entries(&mut file_entries);
        let header = file_entries.remove(header_index);
        self.session_id = header.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        self.header = header;
        self.entries = file_entries
            .into_iter()
            .filter(|entry| entry.get("type").and_then(Value::as_str) != Some("session"))
            .map(Entry::from_raw)
            .collect();
        self.build_index();
        if migrated {
            self.rewrite_file()?;
        }
        Ok(())
    }

    pub(crate) fn build_index(&mut self) {
        self.by_id.clear();
        self.labels.clear();
        self.leaf = None;
        for (index, entry) in self.entries.iter().enumerate() {
            self.by_id.insert(entry.id().to_string(), index);
            self.leaf = Some(entry.id().to_string());
            if let EntryData::Label { target_id, label } = entry.data() {
                match label.as_deref().filter(|label| !label.is_empty()) {
                    Some(label) => {
                        self.labels.insert(target_id.clone(), (label.to_string(), entry.timestamp().to_string()));
                    }
                    None => {
                        self.labels.remove(target_id);
                    }
                }
            }
        }
    }

    fn file_lines(&self) -> Vec<String> {
        let header = serde_json::to_string(&self.header).unwrap_or_default();
        std::iter::once(header).chain(self.entries.iter().map(Entry::to_json_line)).collect()
    }

    /// The whole session as JSONL, exactly as it is (or would be) stored.
    pub fn to_jsonl(&self) -> String {
        self.file_lines().iter().map(|line| format!("{line}\n")).collect()
    }

    pub(crate) fn rewrite_file(&self) -> anyhow::Result<()> {
        match (&self.session_file, self.persist) {
            (Some(path), true) => write_lines(path, &self.file_lines(), false),
            _ => Ok(()),
        }
    }

    /// A file appears once the session holds a user or assistant message, so setup entries
    /// alone leave nothing behind and a first prompt survives a failed first turn.
    pub(crate) fn has_conversation(&self) -> bool {
        self.entries.iter().any(|entry| entry.is_message_role("user") || entry.is_message_role("assistant"))
    }

    fn persist_entry(&mut self, entry_index: usize) -> anyhow::Result<()> {
        let Some(path) = self.session_file.clone().filter(|_| self.persist) else { return Ok(()) };
        if self.flushed {
            return append_line(&path, &self.entries[entry_index].to_json_line());
        }
        if !self.has_conversation() {
            return Ok(());
        }
        write_lines(&path, &self.file_lines(), true)?;
        self.flushed = true;
        Ok(())
    }

    pub(crate) fn append_entry(&mut self, entry: Entry) -> anyhow::Result<String> {
        let id = entry.id().to_string();
        self.by_id.insert(id.clone(), self.entries.len());
        self.entries.push(entry);
        self.leaf = Some(id.clone());
        self.persist_entry(self.entries.len() - 1)?;
        Ok(id)
    }

    pub(crate) fn new_entry_id(&self) -> String {
        ids::generate_entry_id(|id| self.by_id.contains_key(id))
    }

    pub fn is_persisted(&self) -> bool {
        self.persist
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    /// Whether this session lives in the default directory for its cwd.
    pub fn uses_default_session_dir(&self, agent_dir: &Path) -> bool {
        self.session_dir == session_dir(agent_dir, &self.cwd)
    }

    pub fn id(&self) -> &str {
        &self.session_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn file(&self) -> Option<&Path> {
        self.session_file.as_deref()
    }

    pub fn session_file(&self) -> Option<&Path> {
        self.session_file.as_deref()
    }

    /// The typed header.
    pub fn header(&self) -> SessionHeader {
        header_from_raw(&self.header).unwrap_or_default()
    }

    /// The header line as stored.
    pub fn header_raw(&self) -> &FileEntry {
        &self.header
    }

    /// All entries in file order, without the header.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf.as_deref()
    }

    pub fn leaf_entry(&self) -> Option<&Entry> {
        self.leaf.as_deref().and_then(|id| self.get_entry(id))
    }

    pub fn get_entry(&self, id: &str) -> Option<&Entry> {
        self.by_id.get(id).map(|index| &self.entries[*index])
    }

    pub fn get_label(&self, id: &str) -> Option<&str> {
        self.labels.get(id).map(|(label, _)| label.as_str())
    }

    /// The display name from the latest `session_info` entry; an empty name clears it.
    pub fn session_name(&self) -> Option<String> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match entry.data() {
                EntryData::SessionInfo { name } => Some(name.as_deref().map(str::trim).unwrap_or_default().to_string()),
                _ => None,
            })
            .filter(|name| !name.is_empty())
    }

    /// Entries from the root to `from_id` (or the leaf), in path order.
    pub fn get_branch(&self, from_id: Option<&str>) -> Vec<&Entry> {
        let mut path = Vec::new();
        let mut current = from_id.or(self.leaf.as_deref()).and_then(|id| self.get_entry(id));
        while let Some(entry) = current {
            path.push(entry);
            if path.len() > self.entries.len() {
                break;
            }
            current = entry.parent_id().and_then(|parent| self.get_entry(parent));
        }
        path.reverse();
        path
    }

    /// The active, compaction-aware entries for context and rendering.
    pub fn build_context_entries(&self) -> Vec<&Entry> {
        context::context_entries_of_path(self.get_branch(None))
    }

    /// Model context with provenance: which entry produced each message.
    pub fn build_session_projection(&self) -> SessionProjection<'_> {
        context::project_path(self.get_branch(None))
    }

    /// The finalized model context for the current leaf.
    pub fn build_session_context(&self) -> SessionContext {
        self.build_session_projection().into_context()
    }

    /// Alias of [`Self::build_session_context`].
    pub fn context(&self) -> SessionContext {
        self.build_session_context()
    }
}
