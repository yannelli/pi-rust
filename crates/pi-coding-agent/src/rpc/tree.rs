//! The `get_tree` payload. Each entry nests inside its parent, so a long branch is deep JSON:
//! the server writes it without recursion and the client parses it on a large stack, then
//! flattens it into an index-linked node list.

use serde_json::{Map, Value, json};

use crate::session::SessionManager;
use crate::session::tree::SessionTree;

/// Stack for parsing records nested deeper than serde_json's default limit.
const DEEP_PARSE_STACK: usize = 64 << 20;

/// `get_tree` data as JSON text: `{"tree": [node...], "leafId": ...}` where each node is
/// `{entry, children, label?, labelTimestamp?}`.
pub fn tree_data_json(manager: &SessionManager) -> String {
    let tree = manager.get_tree();
    let mut out = String::from("{\"tree\":");
    write_nodes(&tree, &mut out);
    out.push_str(",\"leafId\":");
    out.push_str(&json!(manager.leaf_id()).to_string());
    out.push('}');
    out
}

fn write_nodes(tree: &SessionTree<'_>, out: &mut String) {
    // Frames: sibling list, next sibling, and the node whose children they are.
    let mut stack: Vec<(&[usize], usize, Option<usize>)> = vec![(&tree.roots, 0, None)];
    out.push('[');
    while let Some(&(siblings, next, parent)) = stack.last() {
        if next < siblings.len() {
            let top = stack.len() - 1;
            stack[top].1 += 1;
            if next > 0 {
                out.push(',');
            }
            let index = siblings[next];
            let node = tree.node(index);
            out.push_str("{\"entry\":");
            out.push_str(&Value::Object(node.entry.raw().clone()).to_string());
            out.push_str(",\"children\":[");
            stack.push((&node.children, 0, Some(index)));
            continue;
        }
        stack.pop();
        out.push(']');
        if let Some(index) = parent {
            let node = tree.node(index);
            if let Some(label) = &node.label {
                out.push_str(",\"label\":");
                out.push_str(&json!(label).to_string());
            }
            if let Some(timestamp) = &node.label_timestamp {
                out.push_str(",\"labelTimestamp\":");
                out.push_str(&json!(timestamp).to_string());
            }
            out.push('}');
        }
    }
}

/// Parse one protocol record of any nesting depth.
pub fn parse_json_line(line: &str) -> Result<Value, String> {
    match serde_json::from_str(line) {
        Ok(value) => Ok(value),
        Err(error) if error.to_string().starts_with("recursion limit exceeded") => {
            let line = line.to_string();
            let parse = move || {
                let mut deserializer = serde_json::Deserializer::from_str(&line);
                deserializer.disable_recursion_limit();
                serde::Deserialize::deserialize(&mut deserializer).map_err(|error: serde_json::Error| error.to_string())
            };
            let worker = std::thread::Builder::new().stack_size(DEEP_PARSE_STACK).spawn(parse);
            worker.map_err(|error| error.to_string())?.join().map_err(|_| "JSON parser failed".to_string())?
        }
        Err(error) => Err(error.to_string()),
    }
}

/// One node of [`RpcSessionTree`]; `children` index into its node list.
#[derive(Clone, Debug, PartialEq)]
pub struct RpcTreeNode {
    /// The session entry as stored.
    pub entry: Value,
    pub children: Vec<usize>,
    pub label: Option<String>,
    pub label_timestamp: Option<String>,
}

/// `get_tree` data with the nesting replaced by indices, in depth-first order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RpcSessionTree {
    pub nodes: Vec<RpcTreeNode>,
    pub roots: Vec<usize>,
    pub leaf_id: Option<String>,
}

impl RpcSessionTree {
    /// Flatten `get_tree` data without recursion; nested arrays are moved out, so dropping
    /// the input afterwards stays shallow too.
    pub fn from_data(mut data: Value) -> Result<Self, String> {
        let leaf_id = data.get("leafId").and_then(Value::as_str).map(String::from);
        let Some(Value::Array(roots)) = data.get_mut("tree").map(Value::take) else {
            return Err("get_tree data has no tree".into());
        };
        let mut tree = Self { leaf_id, ..Self::default() };
        let mut pending: Vec<(Value, Option<usize>)> = roots.into_iter().rev().map(|node| (node, None)).collect();
        while let Some((node, parent)) = pending.pop() {
            let Value::Object(mut fields) = node else { return Err("tree node is not an object".into()) };
            let index = tree.nodes.len();
            let children = match fields.remove("children") {
                Some(Value::Array(children)) => children,
                _ => Vec::new(),
            };
            pending.extend(children.into_iter().rev().map(|child| (child, Some(index))));
            tree.nodes.push(RpcTreeNode {
                entry: fields.remove("entry").unwrap_or(Value::Null),
                children: Vec::new(),
                label: string_field(&mut fields, "label"),
                label_timestamp: string_field(&mut fields, "labelTimestamp"),
            });
            match parent {
                Some(parent) => tree.nodes[parent].children.push(index),
                None => tree.roots.push(index),
            }
        }
        Ok(tree)
    }

    /// The entry id of a node.
    pub fn entry_id(&self, index: usize) -> Option<&str> {
        self.nodes.get(index)?.entry.get("id")?.as_str()
    }
}

fn string_field(fields: &mut Map<String, Value>, key: &str) -> Option<String> {
    match fields.remove(key) {
        Some(Value::String(text)) => Some(text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use pi_ai::{Message, UserMessage};

    use super::*;

    fn user(text: &str) -> Message {
        Message::User(UserMessage { content: pi_ai::UserContent::Text(text.into()), timestamp: 1 })
    }

    #[test]
    fn writes_and_flattens_deep_trees() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::in_memory(dir.path());
        let mut ids = Vec::new();
        for index in 0..2000 {
            ids.push(manager.append_message(&user(&format!("m{index}"))).unwrap());
        }
        manager.branch(&ids[0]).unwrap();
        let side = manager.append_message(&user("side")).unwrap();
        manager.append_label_change(&ids[1], Some("mark")).unwrap();
        let line = format!("{{\"type\":\"response\",\"data\":{}}}", tree_data_json(&manager));
        assert!(serde_json::from_str::<Value>(&line).is_err(), "deeper than the default limit");

        let mut record = parse_json_line(&line).unwrap();
        let tree = RpcSessionTree::from_data(record["data"].take()).unwrap();
        assert_eq!(tree.roots.len(), 1);
        assert_eq!(tree.nodes.len(), 2002);
        let root = &tree.nodes[tree.roots[0]];
        assert_eq!(root.children.len(), 2);
        assert_eq!(tree.entry_id(root.children[0]), Some(ids[1].as_str()));
        assert_eq!(tree.entry_id(root.children[1]), Some(side.as_str()));
        assert_eq!(tree.nodes[root.children[0]].label.as_deref(), Some("mark"));
        assert!(tree.nodes[root.children[0]].label_timestamp.is_some());
        assert_eq!(tree.leaf_id.as_deref(), manager.leaf_id());
    }

    #[test]
    fn writes_empty_trees() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::in_memory(dir.path());
        assert_eq!(tree_data_json(&manager), r#"{"tree":[],"leafId":null}"#);
    }
}
