//! Tree navigation: the whole entry tree, children, moving the leaf, and extracting one
//! branch into a new session (`createBranchedSession`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde_json::{Value, json};

use super::entry::{Entry, EntryData, SessionHeader};
use super::ids::{generate_entry_id, iso_timestamp, parse_iso, uuid_v7};
use super::{CURRENT_SESSION_VERSION, SessionManager};

/// One node of [`SessionTree`]; `children` index into the tree's node list.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionTreeNode<'a> {
    pub entry: &'a Entry,
    pub children: Vec<usize>,
    pub label: Option<String>,
    pub label_timestamp: Option<String>,
}

/// The entry tree as a flat node list, so deep sessions need no recursion. Children are
/// ordered oldest first; orphans (missing parent) become roots.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionTree<'a> {
    pub nodes: Vec<SessionTreeNode<'a>>,
    pub roots: Vec<usize>,
}

impl<'a> SessionTree<'a> {
    pub fn node(&self, index: usize) -> &SessionTreeNode<'a> {
        &self.nodes[index]
    }

    /// The node for an entry id.
    pub fn find(&self, id: &str) -> Option<usize> {
        self.nodes.iter().position(|node| node.entry.id() == id)
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}

impl SessionManager {
    /// The whole session as a tree with resolved labels.
    pub fn get_tree(&self) -> SessionTree<'_> {
        let index: HashMap<&str, usize> =
            self.entries.iter().enumerate().map(|(position, entry)| (entry.id(), position)).collect();
        let mut nodes: Vec<SessionTreeNode> = self
            .entries
            .iter()
            .map(|entry| {
                let label = self.labels.get(entry.id());
                SessionTreeNode {
                    entry,
                    children: Vec::new(),
                    label: label.map(|(label, _)| label.clone()),
                    label_timestamp: label.map(|(_, timestamp)| timestamp.clone()),
                }
            })
            .collect();
        let mut roots = Vec::new();
        for (position, entry) in self.entries.iter().enumerate() {
            match entry.parent_id().filter(|parent| *parent != entry.id()).and_then(|parent| index.get(parent)) {
                Some(parent) => nodes[*parent].children.push(position),
                None => roots.push(position),
            }
        }
        let millis: Vec<i64> = self.entries.iter().map(|entry| parse_iso(entry.timestamp()).unwrap_or(0)).collect();
        for node in &mut nodes {
            node.children.sort_by_key(|child| millis[*child]);
        }
        SessionTree { nodes, roots }
    }

    /// Direct children of an entry, in file order.
    pub fn get_children(&self, parent_id: &str) -> Vec<&Entry> {
        self.entries.iter().filter(|entry| entry.parent_id() == Some(parent_id)).collect()
    }

    /// Move the leaf to an earlier entry; the next append starts a new branch there.
    pub fn branch(&mut self, branch_from_id: &str) -> anyhow::Result<()> {
        if self.get_entry(branch_from_id).is_none() {
            anyhow::bail!("Entry {branch_from_id} not found");
        }
        self.leaf = Some(branch_from_id.to_string());
        Ok(())
    }

    /// Move the leaf before the first entry; the next append becomes a new root.
    pub fn reset_leaf(&mut self) {
        self.leaf = None;
    }

    /// Replace this session with a new one holding only the path to `leaf_id` (labels on that
    /// path are rewritten after it). Returns the new file when persisting.
    pub fn create_branched_session(&mut self, leaf_id: &str) -> anyhow::Result<Option<PathBuf>> {
        let previous_file = self.session_file.clone();
        let path: Vec<Entry> = self.get_branch(Some(leaf_id)).into_iter().cloned().collect();
        if path.is_empty() {
            anyhow::bail!("Entry {leaf_id} not found");
        }
        // Labels are tree entries too: drop them and chain later entries to the next kept one.
        let mut kept: Vec<Entry> = Vec::new();
        let mut label_replacement: HashMap<String, String> = HashMap::new();
        let mut pending_labels: Vec<String> = Vec::new();
        let mut parent: Option<String> = None;
        for entry in &path {
            if entry.kind() == "label" {
                pending_labels.push(entry.id().to_string());
                continue;
            }
            for label_id in pending_labels.drain(..) {
                label_replacement.insert(label_id, entry.id().to_string());
            }
            let mut copy = entry.with_parent(parent.as_deref());
            if let EntryData::Compaction(compaction) = entry.data()
                && compaction.first_kept_entry_id != entry.id()
                && let Some(replacement) = label_replacement.get(&compaction.first_kept_entry_id)
            {
                copy = copy.with_field("firstKeptEntryId", Some(json!(replacement)));
            }
            kept.push(copy);
            parent = Some(entry.id().to_string());
        }

        let new_id = uuid_v7();
        let timestamp = iso_timestamp();
        let header = SessionHeader {
            kind: "session".into(),
            version: Some(CURRENT_SESSION_VERSION),
            id: new_id.clone(),
            timestamp: timestamp.clone(),
            cwd: self.cwd.display().to_string(),
            parent_session: previous_file.as_ref().filter(|_| self.persist).map(|file| file.display().to_string()),
        };
        let mut used: HashSet<String> = kept.iter().map(|entry| entry.id().to_string()).collect();
        let mut labels: Vec<(String, String, String)> = self
            .labels
            .iter()
            .filter(|(target, _)| used.contains(*target))
            .map(|(target, (label, stamp))| (target.clone(), label.clone(), stamp.clone()))
            .collect();
        labels.sort_by(|a, b| (parse_iso(&a.2), &a.0).cmp(&(parse_iso(&b.2), &b.0)));
        for (target, label, stamp) in labels {
            let id = generate_entry_id(|candidate| used.contains(candidate));
            used.insert(id.clone());
            kept.push(Entry::build(vec![
                ("type", Some(json!("label"))),
                ("id", Some(json!(id))),
                ("parentId", Some(parent.clone().map_or(Value::Null, Value::String))),
                ("timestamp", Some(json!(stamp))),
                ("targetId", Some(json!(target))),
                ("label", Some(json!(label))),
            ]));
            parent = Some(id);
        }

        self.header = match serde_json::to_value(&header)? {
            Value::Object(object) => object,
            _ => Default::default(),
        };
        self.entries = kept;
        self.session_id = new_id.clone();
        self.build_index();
        if !self.persist {
            return Ok(None);
        }
        let file_timestamp = timestamp.replace([':', '.'], "-");
        let file = self.session_dir.join(format!("{file_timestamp}_{new_id}.jsonl"));
        self.session_file = Some(file.clone());
        // Same rule as appends: write now only if the path already holds a conversation.
        self.flushed = false;
        if self.has_conversation() {
            self.rewrite_file()?;
            self.flushed = true;
        }
        Ok(Some(file))
    }
}
