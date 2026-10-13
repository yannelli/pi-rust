//! The selector's owned copy of the session tree and its flattened, indented rows
//! (upstream `flattenTree` and `recalculateVisualStructure`).

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::session::{Entry, SessionTree};

/// One node of [`TreeData`] (upstream `SessionTreeNode`); `children` index into the node list.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeNode {
    pub entry: Entry,
    pub children: Vec<usize>,
    pub label: Option<String>,
    pub label_timestamp: Option<String>,
}

/// An owned session tree, so the selector outlives the [`SessionTree`] borrow it was built from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeData {
    pub nodes: Vec<TreeNode>,
    pub roots: Vec<usize>,
}

impl TreeData {
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}

impl From<&SessionTree<'_>> for TreeData {
    fn from(tree: &SessionTree<'_>) -> Self {
        let nodes = tree
            .nodes
            .iter()
            .map(|node| TreeNode {
                entry: node.entry.clone(),
                children: node.children.clone(),
                label: node.label.clone(),
                label_timestamp: node.label_timestamp.clone(),
            })
            .collect();
        Self { nodes, roots: tree.roots.clone() }
    }
}

impl From<SessionTree<'_>> for TreeData {
    fn from(tree: SessionTree<'_>) -> Self {
        Self::from(&tree)
    }
}

/// A `│` gutter at the display level where an ancestor's connector was drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Gutter {
    pub position: usize,
    pub show: bool,
}

/// One navigable row: a node plus its indentation, connector, and gutters.
#[derive(Clone, Debug)]
pub(super) struct FlatNode {
    pub node: usize,
    pub indent: usize,
    pub show_connector: bool,
    pub is_last: bool,
    pub gutters: Vec<Gutter>,
    pub is_virtual_root_child: bool,
}

/// Name and arguments of a tool call, for labeling its result.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ToolCallInfo {
    pub name: String,
    pub arguments: Map<String, Value>,
}

/// Stack item of the indentation walks: node, indent, just branched, show connector, is last,
/// gutters, virtual root child.
type StackItem = (usize, usize, bool, bool, bool, Vec<Gutter>, bool);

/// Child indent and gutters shared by both walks: branch points (and the first generation
/// after one) shift right; a displayed connector leaves a gutter for its descendants.
fn child_layout(item: &StackItem, child_count: usize, multiple_roots: bool) -> (usize, Vec<Gutter>) {
    let (_, indent, just_branched, show_connector, is_last, gutters, is_virtual_root_child) = item;
    let child_indent = if child_count > 1 || (*just_branched && *indent > 0) { indent + 1 } else { *indent };
    let mut child_gutters = gutters.clone();
    if *show_connector && !is_virtual_root_child {
        let display_indent = if multiple_roots { indent.saturating_sub(1) } else { *indent };
        child_gutters.push(Gutter { position: display_indent.saturating_sub(1), show: !is_last });
    }
    (child_indent, child_gutters)
}

fn root_items(roots: &[usize], multiple_roots: bool) -> Vec<StackItem> {
    let indent = usize::from(multiple_roots);
    let last = roots.len().saturating_sub(1);
    let items = roots
        .iter()
        .enumerate()
        .map(|(i, &root)| (root, indent, multiple_roots, multiple_roots, i == last, Vec::new(), multiple_roots));
    items.rev().collect()
}

fn push_children(stack: &mut Vec<StackItem>, children: &[usize], indent: usize, gutters: &[Gutter]) {
    let branched = children.len() > 1;
    let last = children.len().saturating_sub(1);
    for (i, &child) in children.iter().enumerate().rev() {
        stack.push((child, indent, branched, branched, i == last, gutters.to_vec(), false));
    }
}

/// Tool calls of an assistant message entry, from its stored JSON.
fn collect_tool_calls(entry: &Entry, tool_calls: &mut HashMap<String, ToolCallInfo>) {
    let Some(message) = entry.raw().get("message") else { return };
    if entry.kind() != "message" || message.get("role").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(content) = message.get("content").and_then(Value::as_array) else { return };
    for block in content.iter().filter(|block| block.get("type").and_then(Value::as_str) == Some("toolCall")) {
        let id = block.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let name = block.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
        let arguments = block.get("arguments").and_then(Value::as_object).cloned().unwrap_or_default();
        tool_calls.insert(id, ToolCallInfo { name, arguments });
    }
}

/// Flattens the tree depth first, ordering the branch with the current leaf first, and
/// collects tool calls for labeling tool results.
pub(super) fn flatten_tree(
    data: &TreeData,
    leaf_id: Option<&str>,
    tool_calls: &mut HashMap<String, ToolCallInfo>,
) -> Vec<FlatNode> {
    tool_calls.clear();
    // Pre-order list, then reversed so children are decided before their parents.
    let mut all_nodes = Vec::new();
    let mut pre_order: Vec<usize> = data.roots.clone();
    while let Some(node) = pre_order.pop() {
        all_nodes.push(node);
        pre_order.extend(data.nodes[node].children.iter().rev());
    }
    let mut contains_active = vec![false; data.nodes.len()];
    for &node in all_nodes.iter().rev() {
        let own = leaf_id.is_some_and(|leaf| data.nodes[node].entry.id() == leaf);
        contains_active[node] = own || data.nodes[node].children.iter().any(|&child| contains_active[child]);
    }

    let multiple_roots = data.roots.len() > 1;
    let mut ordered_roots = data.roots.clone();
    ordered_roots.sort_by_key(|&root| !contains_active[root]);
    let mut stack = root_items(&ordered_roots, multiple_roots);
    let mut result = Vec::new();
    while let Some(item) = stack.pop() {
        let node = item.0;
        collect_tool_calls(&data.nodes[node].entry, tool_calls);
        let children = &data.nodes[node].children;
        let mut ordered: Vec<usize> = children.iter().copied().filter(|&child| contains_active[child]).collect();
        ordered.extend(children.iter().copied().filter(|&child| !contains_active[child]));
        let (child_indent, child_gutters) = child_layout(&item, children.len(), multiple_roots);
        push_children(&mut stack, &ordered, child_indent, &child_gutters);
        let (_, indent, _, show_connector, is_last, gutters, is_virtual_root_child) = item;
        result.push(FlatNode { node, indent, show_connector, is_last, gutters, is_virtual_root_child });
    }
    result
}

/// Visible tree links for navigation: each visible row's nearest visible ancestor, and each
/// row's visible children (`None` for roots), in display order. Values are flat indices.
#[derive(Clone, Debug, Default)]
pub(super) struct VisibleTree {
    pub parent: HashMap<usize, Option<usize>>,
    pub children: HashMap<Option<usize>, Vec<usize>>,
}

impl VisibleTree {
    pub fn children_of(&self, node: Option<usize>) -> &[usize] {
        self.children.get(&node).map(Vec::as_slice).unwrap_or_default()
    }
}

/// Recomputes indentation, connectors, and gutters of the visible rows: hidden entries are
/// skipped and descendants attach to their nearest visible ancestor. Returns the visible tree
/// and whether there are several visible roots.
pub(super) fn recalculate_visual_structure(
    data: &TreeData,
    flat_nodes: &mut [FlatNode],
    flat_index: &HashMap<String, usize>,
    filtered: &[usize],
) -> (VisibleTree, Option<bool>) {
    if filtered.is_empty() {
        return (VisibleTree::default(), None);
    }
    let visible: HashSet<usize> = filtered.iter().copied().collect();
    let parent_of = |flat: usize| -> Option<usize> {
        let entry = &data.nodes[flat_nodes[flat].node].entry;
        entry.parent_id().and_then(|parent| flat_index.get(parent).copied())
    };
    let mut tree = VisibleTree::default();
    tree.children.insert(None, Vec::new());
    for &flat in filtered {
        let mut ancestor = parent_of(flat);
        while let Some(current) = ancestor {
            if visible.contains(&current) {
                break;
            }
            ancestor = parent_of(current);
        }
        tree.parent.insert(flat, ancestor);
        tree.children.entry(ancestor).or_default().push(flat);
    }

    let roots = tree.children_of(None).to_vec();
    let multiple_roots = roots.len() > 1;
    let mut stack = root_items(&roots, multiple_roots);
    while let Some(item) = stack.pop() {
        let children = tree.children_of(Some(item.0)).to_vec();
        let (child_indent, child_gutters) = child_layout(&item, children.len(), multiple_roots);
        push_children(&mut stack, &children, child_indent, &child_gutters);
        let (flat, indent, _, show_connector, is_last, gutters, is_virtual_root_child) = item;
        flat_nodes[flat] =
            FlatNode { node: flat_nodes[flat].node, indent, show_connector, is_last, gutters, is_virtual_root_child };
    }
    (tree, Some(multiple_roots))
}
