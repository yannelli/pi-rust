//! Position maps from transpiled code back to the original source, and stack trace rewriting.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use serde::{Deserialize, Serialize};

/// A 1-based line and column in the original file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourcePosition {
    pub line: u32,
    pub column: u32,
}

/// Mapping tokens `[generated line, generated column, original line, original column]`, 0-based
/// and sorted by generated position.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PositionMap {
    tokens: Vec<[u32; 4]>,
}

impl PositionMap {
    pub(crate) fn from_source_map(map: &oxc_sourcemap::SourceMap<'_>) -> Self {
        let mut tokens: Vec<[u32; 4]> = map
            .get_tokens()
            .map(|token| [token.get_dst_line(), token.get_dst_col(), token.get_src_line(), token.get_src_col()])
            .collect();
        tokens.sort_unstable();
        Self { tokens }
    }

    /// Map a 1-based generated position to the original one.
    pub fn original(&self, line: u32, column: u32) -> Option<SourcePosition> {
        let key = [line.checked_sub(1)?, column.saturating_sub(1), u32::MAX, u32::MAX];
        let index = self.tokens.partition_point(|token| token <= &key);
        let token = self.tokens.get(index.checked_sub(1)?)?;
        if token[0] != key[0] {
            // No token earlier on this line: use the line's first token.
            let first = self.tokens.get(index).filter(|token| token[0] == key[0])?;
            return Some(SourcePosition { line: first[2] + 1, column: first[3] + 1 });
        }
        Some(SourcePosition { line: token[2] + 1, column: token[3] + 1 + (key[1] - token[1]) })
    }
}

struct Registered {
    map: Option<Rc<PositionMap>>,
    /// Characters the loader put before the code on the first line.
    first_line_offset: u32,
}

thread_local! {
    static REGISTRY: RefCell<HashMap<String, Registered>> = RefCell::new(HashMap::new());
}

/// Record how the module named `file` maps back to its source, for stack rewriting.
pub(crate) fn register_source_map(file: &str, map: Option<Rc<PositionMap>>, first_line_offset: u32) {
    REGISTRY.with(|registry| {
        registry.borrow_mut().insert(file.to_string(), Registered { map, first_line_offset });
    });
}

/// Rewrite `file:line:col` locations in QuickJS stack frames to original source positions.
pub(crate) fn remap_stack(stack: &str) -> String {
    REGISTRY.with(|registry| {
        let registry = registry.borrow();
        if registry.is_empty() {
            return stack.to_string();
        }
        stack.lines().map(|line| remap_frame(line, &registry)).collect::<Vec<_>>().join("\n")
    })
}

fn remap_frame(line: &str, registry: &HashMap<String, Registered>) -> String {
    let (head, location, tail) = match line.rfind('(') {
        Some(open) if line.ends_with(')') => (&line[..=open], &line[open + 1..line.len() - 1], ")"),
        _ => match line.find("at ") {
            Some(at) => (&line[..at + 3], &line[at + 3..], ""),
            None => return line.to_string(),
        },
    };
    let Some((file, line_number, column)) = split_location(location) else {
        return line.to_string();
    };
    let Some(entry) = registry.get(file) else {
        return line.to_string();
    };
    let column = if line_number == 1 { column.saturating_sub(entry.first_line_offset).max(1) } else { column };
    let position = match &entry.map {
        Some(map) => map.original(line_number, column),
        None => Some(SourcePosition { line: line_number, column }),
    };
    match position {
        Some(position) => format!("{head}{file}:{}:{}{tail}", position.line, position.column),
        None => line.to_string(),
    }
}

fn split_location(location: &str) -> Option<(&str, u32, u32)> {
    let (rest, column) = location.rsplit_once(':')?;
    let (file, line) = rest.rsplit_once(':')?;
    Some((file, line.parse().ok()?, column.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_generated_positions_to_original_ones() {
        let map = PositionMap { tokens: vec![[0, 0, 2, 0], [0, 6, 2, 10], [3, 2, 7, 4]] };
        assert_eq!(map.original(1, 1), Some(SourcePosition { line: 3, column: 1 }));
        assert_eq!(map.original(1, 8), Some(SourcePosition { line: 3, column: 12 }));
        assert_eq!(map.original(4, 1), Some(SourcePosition { line: 8, column: 5 }));
        assert_eq!(map.original(9, 1), None);
    }

    #[test]
    fn rewrites_registered_frames_only() {
        let map = PositionMap { tokens: vec![[1, 0, 4, 2]] };
        register_source_map("/x/a.ts", Some(Rc::new(map)), 0);
        register_source_map("/x/b.js", None, 10);
        let stack = "    at f (/x/a.ts:2:5)\n    at /x/b.js:1:15\n    at g (/x/c.js:1:1)";
        assert_eq!(remap_stack(stack), "    at f (/x/a.ts:5:7)\n    at /x/b.js:1:5\n    at g (/x/c.js:1:1)");
    }
}
