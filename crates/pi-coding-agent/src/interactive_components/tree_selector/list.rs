//! [`TreeList`]: the navigable tree rows with filters, search, folding, and branch jumps
//! (upstream `TreeList`).

use std::collections::{HashMap, HashSet};

use pi_tui::get_keybindings;
use serde_json::Value;

use super::nodes::{
    FlatNode, ToolCallInfo, TreeData, TreeNode, VisibleTree, flatten_tree, recalculate_visual_structure,
};
use super::text::{entry_copy_text, has_text_content, searchable_text};
use crate::session::iso_timestamp;
use crate::settings::schema::TreeFilterMode;

/// Receives an entry id.
pub type EntryCallback = Box<dyn FnMut(&str)>;
/// Receives the copied text, or `None` when the entry has none.
pub type CopyCallback = Box<dyn FnMut(Option<&str>)>;
/// Receives an entry id and its label (`None` when cleared or absent).
pub type LabelCallback = Box<dyn FnMut(&str, Option<&str>)>;

/// Filter mode of the tree view (upstream `FilterMode`).
pub type FilterMode = TreeFilterMode;

const FILTER_CYCLE: [FilterMode; 5] =
    [FilterMode::Default, FilterMode::NoTools, FilterMode::UserOnly, FilterMode::LabeledOnly, FilterMode::All];

/// Entry types hidden in the default view (settings and bookkeeping).
const SETTINGS_ENTRY_TYPES: [&str; 6] =
    ["label", "context_edit", "custom", "model_change", "thinking_level_change", "session_info"];

/// Tree rows with selection. Callbacks run from [`pi_tui::Component::handle_input`].
pub struct TreeList {
    pub(super) data: TreeData,
    pub(super) flat_nodes: Vec<FlatNode>,
    flat_index: HashMap<String, usize>,
    /// Visible rows as indices into `flat_nodes`.
    pub(super) filtered: Vec<usize>,
    pub(super) selected_index: usize,
    current_leaf_id: Option<String>,
    pub(super) max_visible_lines: usize,
    pub(super) filter_mode: FilterMode,
    search_query: String,
    pub(super) tool_calls: HashMap<String, ToolCallInfo>,
    pub(super) multiple_roots: bool,
    pub(super) show_label_timestamps: bool,
    pub(super) active_path: HashSet<String>,
    visible: VisibleTree,
    last_selected_id: Option<String>,
    pub(super) folded: HashSet<String>,
    pub on_select: Option<EntryCallback>,
    pub on_cancel: Option<Box<dyn FnMut()>>,
    pub on_copy: Option<CopyCallback>,
    pub on_label_edit: Option<LabelCallback>,
}

impl TreeList {
    pub fn new(
        tree: impl Into<TreeData>,
        current_leaf_id: Option<&str>,
        max_visible_lines: usize,
        initial_selected_id: Option<&str>,
        initial_filter_mode: Option<FilterMode>,
    ) -> Self {
        let data = tree.into();
        let mut tool_calls = HashMap::new();
        let flat_nodes = flatten_tree(&data, current_leaf_id, &mut tool_calls);
        let flat_index = flat_nodes
            .iter()
            .enumerate()
            .map(|(index, flat)| (data.nodes[flat.node].entry.id().to_string(), index))
            .collect();
        let mut list = Self {
            multiple_roots: data.roots.len() > 1,
            data,
            flat_nodes,
            flat_index,
            filtered: Vec::new(),
            selected_index: 0,
            current_leaf_id: current_leaf_id.map(String::from),
            max_visible_lines,
            filter_mode: initial_filter_mode.unwrap_or_default(),
            search_query: String::new(),
            tool_calls,
            show_label_timestamps: false,
            active_path: HashSet::new(),
            visible: VisibleTree::default(),
            last_selected_id: None,
            folded: HashSet::new(),
            on_select: None,
            on_cancel: None,
            on_copy: None,
            on_label_edit: None,
        };
        list.build_active_path();
        list.apply_filter();
        let target = initial_selected_id.or(current_leaf_id).map(String::from);
        list.selected_index = list.find_nearest_visible_index(target.as_deref());
        list.last_selected_id = list.selected_id();
        list
    }

    pub(super) fn node_of(&self, flat: usize) -> &TreeNode {
        &self.data.nodes[self.flat_nodes[flat].node]
    }

    fn id_of(&self, flat: usize) -> &str {
        self.node_of(flat).entry.id()
    }

    fn parent_flat(&self, flat: usize) -> Option<usize> {
        self.node_of(flat).entry.parent_id().and_then(|parent| self.flat_index.get(parent).copied())
    }

    fn selected_id(&self) -> Option<String> {
        self.filtered.get(self.selected_index).map(|&flat| self.id_of(flat).to_string())
    }

    fn row_positions(&self) -> HashMap<usize, usize> {
        self.filtered.iter().enumerate().map(|(row, &flat)| (flat, row)).collect()
    }

    /// The visible row of `entry_id` or of its nearest visible ancestor, else the last row.
    fn find_nearest_visible_index(&self, entry_id: Option<&str>) -> usize {
        if self.filtered.is_empty() {
            return 0;
        }
        let rows = self.row_positions();
        let mut current = entry_id.and_then(|id| self.flat_index.get(id).copied());
        while let Some(flat) = current {
            if let Some(&row) = rows.get(&flat) {
                return row;
            }
            current = self.parent_flat(flat);
        }
        self.filtered.len() - 1
    }

    /// Ids on the path from the root to the current leaf.
    fn build_active_path(&mut self) {
        self.active_path.clear();
        let Some(leaf) = self.current_leaf_id.clone().filter(|leaf| !leaf.is_empty()) else { return };
        self.active_path.insert(leaf.clone());
        let mut current = self.flat_index.get(&leaf).copied();
        while let Some(flat) = current {
            let Some(parent) = self.node_of(flat).entry.parent_id().filter(|parent| !parent.is_empty()) else {
                break;
            };
            let parent = parent.to_string();
            current = self.flat_index.get(&parent).copied();
            self.active_path.insert(parent);
        }
    }

    fn passes_filter(&self, flat: usize, tokens: &[String]) -> bool {
        let node = self.node_of(flat);
        let entry = &node.entry;
        let kind = entry.kind();
        if kind == "usage" {
            return false;
        }
        let message = entry.raw().get("message");
        let role = message.and_then(|message| message.get("role")).and_then(Value::as_str);
        let is_message = |wanted: &str| kind == "message" && role == Some(wanted);
        // Assistant messages with only tool calls are hidden unless they failed or are the leaf.
        if is_message("assistant")
            && self.current_leaf_id.as_deref() != Some(entry.id())
            && let Some(message) = message
        {
            let stop_reason = message.get("stopReason").and_then(Value::as_str).unwrap_or_default();
            let failed = !stop_reason.is_empty() && stop_reason != "stop" && stop_reason != "toolUse";
            if !has_text_content(message.get("content")) && !failed {
                return false;
            }
        }
        let is_settings = SETTINGS_ENTRY_TYPES.contains(&kind);
        let passes = match self.filter_mode {
            FilterMode::UserOnly => is_message("user"),
            FilterMode::NoTools => !is_settings && !is_message("toolResult"),
            FilterMode::LabeledOnly => node.label.is_some(),
            FilterMode::All => true,
            FilterMode::Default => !is_settings,
        };
        if !passes {
            return false;
        }
        if tokens.is_empty() {
            return true;
        }
        let text = searchable_text(node).to_lowercase();
        tokens.iter().all(|token| text.contains(token.as_str()))
    }

    pub(super) fn apply_filter(&mut self) {
        // Remember the selection only while rows exist, so empty filters round-trip.
        if !self.filtered.is_empty() {
            self.last_selected_id = self.selected_id().or(self.last_selected_id.take());
        }
        let tokens: Vec<String> = self.search_query.to_lowercase().split_whitespace().map(String::from).collect();
        let mut filtered: Vec<usize> =
            (0..self.flat_nodes.len()).filter(|&flat| self.passes_filter(flat, &tokens)).collect();

        if !self.folded.is_empty() {
            let mut skip: HashSet<String> = HashSet::new();
            for flat in 0..self.flat_nodes.len() {
                let entry = &self.node_of(flat).entry;
                if let Some(parent) = entry.parent_id()
                    && (self.folded.contains(parent) || skip.contains(parent))
                {
                    skip.insert(entry.id().to_string());
                }
            }
            filtered.retain(|&flat| !skip.contains(self.id_of(flat)));
        }
        self.filtered = filtered;

        let (visible, multiple_roots) =
            recalculate_visual_structure(&self.data, &mut self.flat_nodes, &self.flat_index, &self.filtered);
        if let Some(multiple_roots) = multiple_roots {
            self.visible = visible;
            self.multiple_roots = multiple_roots;
        }

        if let Some(last) = self.last_selected_id.clone() {
            self.selected_index = self.find_nearest_visible_index(Some(&last));
        } else if self.selected_index >= self.filtered.len() {
            self.selected_index = self.filtered.len().saturating_sub(1);
        }
        if !self.filtered.is_empty() {
            self.last_selected_id = self.selected_id().or(self.last_selected_id.take());
        }
    }

    pub fn get_search_query(&self) -> &str {
        &self.search_query
    }

    pub fn get_filter_mode(&self) -> FilterMode {
        self.filter_mode
    }

    pub fn shows_label_timestamps(&self) -> bool {
        self.show_label_timestamps
    }

    /// The selected node, if any row is visible.
    pub fn get_selected_node(&self) -> Option<&TreeNode> {
        self.filtered.get(self.selected_index).map(|&flat| self.node_of(flat))
    }

    /// Ids of the visible rows, top to bottom.
    pub fn visible_ids(&self) -> Vec<&str> {
        self.filtered.iter().map(|&flat| self.id_of(flat)).collect()
    }

    /// Reports the selected entry's full text (or `None`) to `on_copy`.
    pub fn copy_selected(&mut self) {
        let text = self.get_selected_node().and_then(entry_copy_text);
        if let Some(on_copy) = self.on_copy.as_mut() {
            on_copy(text.as_deref());
        }
    }

    /// Sets or clears a node's label; a new label is stamped `label_timestamp` or now.
    pub fn update_node_label(&mut self, entry_id: &str, label: Option<&str>, label_timestamp: Option<&str>) {
        let Some(&flat) = self.flat_index.get(entry_id) else { return };
        let node = &mut self.data.nodes[self.flat_nodes[flat].node];
        node.label = label.map(String::from);
        node.label_timestamp =
            label.filter(|label| !label.is_empty()).map(|_| label_timestamp.map_or_else(iso_timestamp, String::from));
    }

    /// A foldable node has visible children and is a root or the first node of a branch.
    pub(super) fn is_foldable(&self, flat: usize) -> bool {
        if self.visible.children_of(Some(flat)).is_empty() {
            return false;
        }
        match self.visible.parent.get(&flat).copied().flatten() {
            None => true,
            Some(parent) => self.visible.children_of(Some(parent)).len() > 1,
        }
    }

    /// The row of the next branch segment start: up walks visible parents, down follows the
    /// first visible child until a branch point or a leaf.
    fn find_branch_segment_start(&self, up: bool) -> usize {
        let Some(&selected) = self.filtered.get(self.selected_index) else { return self.selected_index };
        let rows = self.row_positions();
        let row_of = |flat: usize| rows.get(&flat).copied().unwrap_or(self.selected_index);
        let mut current = selected;
        loop {
            if up {
                let Some(parent) = self.visible.parent.get(&current).copied().flatten() else {
                    return row_of(current);
                };
                if self.visible.children_of(Some(parent)).len() > 1 && row_of(current) < self.selected_index {
                    return row_of(current);
                }
                current = parent;
            } else {
                match self.visible.children_of(Some(current)) {
                    [] => return row_of(current),
                    [only] => current = *only,
                    [first, ..] => return row_of(*first),
                }
            }
        }
    }

    fn set_filter(&mut self, mode: FilterMode) {
        self.filter_mode = mode;
        self.folded.clear();
        self.apply_filter();
    }

    fn toggle_filter(&mut self, mode: FilterMode) {
        self.set_filter(if self.filter_mode == mode { FilterMode::Default } else { mode });
    }

    fn cycle_filter(&mut self, step: usize) {
        let current = FILTER_CYCLE.iter().position(|mode| *mode == self.filter_mode).unwrap_or(0);
        self.set_filter(FILTER_CYCLE[(current + step) % FILTER_CYCLE.len()]);
    }

    fn set_search(&mut self, query: String) {
        self.search_query = query;
        self.folded.clear();
        self.apply_filter();
    }

    pub(super) fn handle_key(&mut self, data: &str) {
        let kb = get_keybindings();
        let len = self.filtered.len();
        let selected = self.filtered.get(self.selected_index).copied();
        let selected_id = selected.map(|flat| self.id_of(flat).to_string());
        if kb.matches(data, "tui.select.up") {
            self.selected_index =
                if self.selected_index == 0 { len.saturating_sub(1) } else { self.selected_index - 1 };
        } else if kb.matches(data, "tui.select.down") {
            self.selected_index = if self.selected_index + 1 == len { 0 } else { self.selected_index + 1 };
        } else if kb.matches(data, "app.tree.foldOrUp") {
            match (selected, selected_id) {
                (Some(flat), Some(id)) if self.is_foldable(flat) && !self.folded.contains(&id) => {
                    self.folded.insert(id);
                    self.apply_filter();
                }
                _ => self.selected_index = self.find_branch_segment_start(true),
            }
        } else if kb.matches(data, "app.tree.unfoldOrDown") {
            match selected_id {
                Some(id) if self.folded.contains(&id) => {
                    self.folded.remove(&id);
                    self.apply_filter();
                }
                _ => self.selected_index = self.find_branch_segment_start(false),
            }
        } else if kb.matches(data, "tui.editor.cursorLeft") || kb.matches(data, "tui.select.pageUp") {
            self.selected_index = self.selected_index.saturating_sub(self.max_visible_lines);
        } else if kb.matches(data, "tui.editor.cursorRight") || kb.matches(data, "tui.select.pageDown") {
            self.selected_index = (self.selected_index + self.max_visible_lines).min(len.saturating_sub(1));
        } else if kb.matches(data, "tui.select.confirm") {
            if let (Some(id), Some(on_select)) = (selected_id, self.on_select.as_mut()) {
                on_select(&id);
            }
        } else if kb.matches(data, "app.message.copy") {
            self.copy_selected();
        } else if kb.matches(data, "tui.select.cancel") {
            if !self.search_query.is_empty() {
                self.set_search(String::new());
            } else if let Some(on_cancel) = self.on_cancel.as_mut() {
                on_cancel();
            }
        } else if kb.matches(data, "app.tree.filter.default") {
            self.set_filter(FilterMode::Default);
        } else if kb.matches(data, "app.tree.filter.noTools") {
            self.toggle_filter(FilterMode::NoTools);
        } else if kb.matches(data, "app.tree.filter.userOnly") {
            self.toggle_filter(FilterMode::UserOnly);
        } else if kb.matches(data, "app.tree.filter.labeledOnly") {
            self.toggle_filter(FilterMode::LabeledOnly);
        } else if kb.matches(data, "app.tree.filter.all") {
            self.toggle_filter(FilterMode::All);
        } else if kb.matches(data, "app.tree.filter.cycleBackward") {
            self.cycle_filter(FILTER_CYCLE.len() - 1);
        } else if kb.matches(data, "app.tree.filter.cycleForward") {
            self.cycle_filter(1);
        } else if kb.matches(data, "tui.editor.deleteCharBackward") {
            if !self.search_query.is_empty() {
                let mut query = self.search_query.clone();
                query.pop();
                self.set_search(query);
            }
        } else if kb.matches(data, "app.tree.editLabel") {
            if let (Some(flat), Some(on_label_edit)) = (selected, self.on_label_edit.as_mut()) {
                let node = &self.data.nodes[self.flat_nodes[flat].node];
                on_label_edit(node.entry.id(), node.label.as_deref());
            }
        } else if kb.matches(data, "app.tree.toggleLabelTimestamp") {
            self.show_label_timestamps = !self.show_label_timestamps;
        } else {
            let control = data.chars().any(|ch| ch < ' ' || ch == '\x7f' || ('\u{80}'..='\u{9f}').contains(&ch));
            if !control && !data.is_empty() {
                self.set_search(format!("{}{data}", self.search_query));
            }
        }
    }
}
