//! Incremental, tolerant parsing of JSON text that arrives in pieces, such as streamed tool-call
//! arguments (upstream `utils/streaming-json.ts`). Each piece is read once, so a stream costs time
//! linear in its length. [`StreamingJsonParser`] documents the rules for incomplete and malformed text.

mod parser;
mod scalars;

use std::sync::Arc;

use serde_json::Value;

use crate::json::parse_tool_call_arguments;
use crate::types::JsonObject;

pub use parser::StreamingJsonParser;

/// One step of a [`JsonPath`]: an object member key or an array index.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

/// A path from the root value. It is shared, so reporting a change never copies it.
pub type JsonPath = Arc<[PathSegment]>;

/// One change a piece of text made to the value, for a writer that keeps its own copy.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonChange {
    /// A key, an array item, or a finished value appeared. A new string is set once with its first
    /// text, a container empty; a container nested more than 32 levels deep is set whole when it closes.
    Set { path: JsonPath, value: Value },
    /// The piece extended the open string at `path`.
    Append { path: JsonPath, text: String },
}

impl JsonChange {
    /// Apply this change to `target`, a copy of the value kept current by applying every change.
    pub fn apply(&self, target: &mut Option<Value>) {
        match self {
            Self::Set { path, value } => set_at(target, path, value.clone()),
            Self::Append { path, text } => append_at(target, path, text),
        }
    }
}

/// The value at `path` in `root`.
fn value_at<'a>(root: &'a mut Option<Value>, path: &[PathSegment]) -> Option<&'a mut Value> {
    let mut current = root.as_mut()?;
    for segment in path {
        current = match segment {
            PathSegment::Key(key) => current.as_object_mut()?.get_mut(key)?,
            PathSegment::Index(index) => current.as_array_mut()?.get_mut(*index)?,
        };
    }
    Some(current)
}

fn set_at(root: &mut Option<Value>, path: &[PathSegment], value: Value) {
    let Some((last, parent)) = path.split_last() else {
        *root = Some(value);
        return;
    };
    match (value_at(root, parent), last) {
        (Some(Value::Object(members)), PathSegment::Key(key)) => {
            members.insert(key.clone(), value);
        }
        (Some(Value::Array(items)), PathSegment::Index(index)) => set_item(items, *index, value),
        _ => {}
    }
}

fn set_item(items: &mut Vec<Value>, index: usize, value: Value) {
    if index == items.len() {
        items.push(value);
    } else if let Some(item) = items.get_mut(index) {
        *item = value;
    }
}

fn append_at(root: &mut Option<Value>, path: &[PathSegment], text: &str) {
    if let Some(Value::String(string)) = value_at(root, path) {
        string.push_str(text);
    }
}

/// The arguments of one streaming tool call: the raw text, for the one full parse when the call
/// ends, and a [`StreamingJsonParser`] for the partial arguments in between. Providers keep one per
/// call in their stream state, never on the content block.
#[derive(Debug, Default)]
pub struct ToolCallArgumentsStream {
    text: String,
    parser: StreamingJsonParser,
    empty: JsonObject,
}

impl ToolCallArgumentsStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// The raw argument text received so far.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Append a piece of argument text; returns the partial arguments.
    pub fn append(&mut self, delta: &str) -> &JsonObject {
        self.text.push_str(delta);
        self.parser.append(delta);
        self.partial()
    }

    /// Take the provider's complete argument text. When it continues the text so far, returns the
    /// missing piece; otherwise starts over with `text` and returns `None`. Either way the second
    /// value is the partial arguments of `text`.
    pub fn replace(&mut self, text: &str) -> (Option<String>, &JsonObject) {
        if let Some(delta) = text.strip_prefix(self.text.as_str()) {
            let delta = delta.to_string();
            return (Some(delta.clone()), self.append(&delta));
        }
        self.text.clear();
        self.parser = StreamingJsonParser::new();
        (None, self.append(text))
    }

    /// The final arguments: one full parse of the complete text, which reports malformed arguments.
    pub fn finish(&self) -> (JsonObject, Option<String>) {
        parse_tool_call_arguments(&self.text)
    }

    /// The partial arguments. Tool arguments are an object; any other value shows as no arguments yet.
    pub fn partial(&mut self) -> &JsonObject {
        match self.parser.snapshot() {
            Some(Value::Object(arguments)) => arguments,
            _ => &self.empty,
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_fuzz;
