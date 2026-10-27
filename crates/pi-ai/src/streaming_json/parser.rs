//! The incremental parser. Containers within 32 levels live in `value` and change in place as the
//! parser reports changes; deeper ones are built aside and set whole when they close.

use serde_json::{Map, Value};

use super::scalars::{is_delimiter, scalar_value, simple_escape};
use super::{JsonChange, JsonPath, PathSegment, append_at, set_at, set_item};
use pending::Overlay;

mod pending;

// Changes are reported at most this deep, so a path costs O(1) to build and to follow.
const MAX_REPORTED_DEPTH: usize = 32;
// Deeper containers are dropped: serde_json rejects them in the final parse, and keeping them
// would make dropping or cloning the value recurse without bound.
const MAX_NESTING: usize = 128;

/// Where a value sits in its parent. `None` in its place marks a value in an invalid position.
#[derive(Clone, Debug)]
enum Slot {
    Root,
    Member(String),
    Element(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Key,
    Colon,
    Value,
    Comma,
}

/// An open object or array.
#[derive(Debug)]
struct Frame {
    is_array: bool,
    slot: Option<Slot>,
    /// Where the container sits, when its changes are reported.
    path: Option<JsonPath>,
    /// The container itself when changes inside it are not reported; otherwise it lives in `value`.
    own: Option<Value>,
    /// Arrays: the element count, where the next element goes.
    len: usize,
    /// Objects: the key read last, where the member value goes.
    key: Option<String>,
    phase: Phase,
}

/// An unterminated string.
#[derive(Debug)]
struct OpenString {
    is_key: bool,
    slot: Option<Slot>,
    path: Option<JsonPath>,
    /// The text of a key or of a value whose changes are not reported; reported ones live in `value`.
    text: String,
    /// The escape read so far: empty for none, else a backslash and up to five more characters.
    escape: String,
    /// A decoded high surrogate escape waiting for its low half.
    high_surrogate: Option<u32>,
    /// Index in `changes` of the change this string extends.
    change: Option<usize>,
}

/// An unterminated number or literal.
#[derive(Debug)]
struct Scalar {
    token: String,
    slot: Option<Slot>,
}

/// Incremental, tolerant JSON parser: [`append`](Self::append) reads each piece once and returns its
/// changes, [`snapshot`](Self::snapshot) the value read so far. It reads incomplete text the way
/// [`crate::json::parse_streaming_json`] does and malformed text the way `repair_json` repairs it.
#[derive(Debug, Default)]
pub struct StreamingJsonParser {
    value: Option<Value>,
    done: bool,
    stack: Vec<Frame>,
    string: Option<OpenString>,
    scalar: Option<Scalar>,
    /// Open containers inside a dropped container (invalid position or too deep).
    dropped_depth: usize,
    changes: Vec<JsonChange>,
    overlay: Option<Overlay>,
    /// Whether `snapshot()` has shown the unterminated scalar since the last piece.
    shown: bool,
}

impl StreamingJsonParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the next piece of text; returns the changes it made, in order. Costs time linear in the
    /// length of `text`. Unterminated numbers and literals are not reported until they end.
    pub fn append(&mut self, text: &str) -> &[JsonChange] {
        self.hide_pending_scalar();
        self.changes.clear();
        if let Some(open) = &mut self.string {
            open.change = None;
        }
        let bytes = text.as_bytes();
        let mut index = 0;
        while index < bytes.len() && !self.done {
            index = if let Some(open) = self.string.take() {
                self.read_string(open, text, index)
            } else if self.scalar.is_some() {
                self.read_scalar(text, index)
            } else {
                self.structural(bytes[index], index)
            };
        }
        &self.changes
    }

    /// The value of the text read so far, `None` before a value starts. An unterminated literal shows
    /// once it is a prefix of `true`, `false` or `null`, a number once it reads as one.
    pub fn snapshot(&mut self) -> Option<&Value> {
        if !self.shown {
            self.shown = true;
            self.show_pending_scalar();
        }
        self.value.as_ref()
    }

    /// Read string bytes from `start` until the string ends or the text does; returns the next index.
    fn read_string(&mut self, mut open: OpenString, text: &str, start: usize) -> usize {
        let bytes = text.as_bytes();
        let mut run = start;
        let mut index = start;
        while index < bytes.len() {
            let byte = bytes[index];
            if !open.escape.is_empty() {
                if self.continue_escape(&mut open, byte) {
                    index += 1;
                    run = index;
                    continue;
                }
                // Not part of the escape: read it as a normal string character below.
                run = index;
            }
            if byte == b'"' {
                self.append_text(&mut open, &text[run..index]);
                self.end_string(open);
                return index + 1;
            }
            if byte == b'\\' {
                self.append_text(&mut open, &text[run..index]);
                open.escape.push('\\');
                run = index + 1;
            }
            index += 1;
        }
        self.append_text(&mut open, &text[run..]);
        self.string = Some(open);
        bytes.len()
    }

    /// Continue an escape with `byte`; returns false when `byte` is not part of it.
    fn continue_escape(&mut self, open: &mut OpenString, byte: u8) -> bool {
        if open.escape.len() == 1 {
            if let Some(decoded) = simple_escape(byte) {
                open.escape.clear();
                self.append_text(open, decoded);
                return true;
            }
            if byte == b'u' {
                open.escape.push('u');
                return true;
            }
            // Invalid escape: keep the backslash as text, as repair_json() does.
            open.escape.clear();
            self.append_text(open, "\\");
            return false;
        }
        if byte.is_ascii_hexdigit() {
            open.escape.push(char::from(byte));
            if open.escape.len() < 6 {
                return true;
            }
            let code = u32::from_str_radix(&open.escape[2..], 16).unwrap_or_default();
            open.escape.clear();
            self.append_code_unit(open, code);
            return true;
        }
        // `\u` without four hex digits: keep it as text, as repair_json() does.
        let literal = std::mem::take(&mut open.escape);
        self.append_text(open, &literal);
        false
    }

    /// Rust strings cannot hold a lone surrogate: a high surrogate shows once its low half arrives,
    /// and unpaired ones are dropped, as repair_json() drops them.
    fn append_code_unit(&mut self, open: &mut OpenString, code: u32) {
        let decoded = match code {
            0xD800..=0xDBFF => {
                open.high_surrogate = Some(code);
                return;
            }
            0xDC00..=0xDFFF => match open.high_surrogate.take() {
                Some(high) => char::from_u32(0x10000 + ((high - 0xD800) << 10) + (code - 0xDC00)),
                None => None,
            },
            _ => char::from_u32(code),
        };
        if let Some(decoded) = decoded {
            self.append_text(open, decoded.encode_utf8(&mut [0; 4]));
        }
    }

    fn append_text(&mut self, open: &mut OpenString, piece: &str) {
        if piece.is_empty() {
            return;
        }
        open.high_surrogate = None;
        let Some(path) = open.path.clone() else {
            open.text.push_str(piece);
            return;
        };
        append_at(&mut self.value, &path, piece);
        // Extend this string's change while it is the newest one; otherwise report an append.
        let newest = open.change.filter(|index| index + 1 == self.changes.len());
        match newest.and_then(|index| self.changes.get_mut(index)) {
            Some(JsonChange::Set { value: Value::String(text), .. } | JsonChange::Append { text, .. }) => {
                text.push_str(piece)
            }
            _ => {
                self.changes.push(JsonChange::Append { path, text: piece.to_string() });
                open.change = Some(self.changes.len() - 1);
            }
        }
    }

    fn start_string(&mut self) {
        // A missing comma is tolerated: `{"a": 1 "b": 2}` reads the second member.
        let is_key = self.dropped_depth == 0
            && self
                .stack
                .last()
                .is_some_and(|frame| !frame.is_array && matches!(frame.phase, Phase::Key | Phase::Comma));
        let mut open = OpenString {
            is_key,
            slot: None,
            path: None,
            text: String::new(),
            escape: String::new(),
            high_surrogate: None,
            change: None,
        };
        if !is_key {
            open.slot = self.value_slot();
            open.path = self.reported_path(open.slot.as_ref());
            if open.path.is_some() {
                open.change = self.set(open.slot.clone(), open.path.clone(), Value::String(String::new()));
            }
        }
        self.string = Some(open);
    }

    fn end_string(&mut self, open: OpenString) {
        if open.is_key {
            if let Some(frame) = self.stack.last_mut() {
                frame.key = Some(open.text);
                frame.phase = Phase::Colon;
            }
            return;
        }
        if open.path.is_none() {
            self.set(open.slot, None, Value::String(open.text));
        }
        self.value_ended();
    }

    /// Read number or literal bytes from `start` up to the next delimiter; returns its index.
    fn read_scalar(&mut self, text: &str, start: usize) -> usize {
        let bytes = text.as_bytes();
        let end = bytes[start..].iter().position(|byte| is_delimiter(*byte)).map_or(bytes.len(), |end| start + end);
        if let Some(scalar) = &mut self.scalar {
            scalar.token.push_str(&text[start..end]);
        }
        if end < bytes.len() {
            self.end_scalar();
        }
        end
    }

    fn end_scalar(&mut self) {
        let Some(Scalar { token, slot }) = self.scalar.take() else { return };
        if let Some(value) = scalar_value(&token) {
            let path = self.reported_path(slot.as_ref());
            self.set(slot, path, value);
        }
        self.value_ended();
    }

    /// Handle one byte outside strings and scalars; returns the next index.
    fn structural(&mut self, byte: u8, index: usize) -> usize {
        match byte {
            b' ' | b'\t' | b'\n' | b'\r' => {}
            b'"' => self.start_string(),
            b'{' | b'[' => self.open_container(byte == b'['),
            b'}' | b']' => self.close_container(),
            b':' | b',' if self.dropped_depth > 0 => {}
            b':' => {
                if let Some(frame) = self.stack.last_mut()
                    && !frame.is_array
                    && frame.phase == Phase::Colon
                {
                    frame.phase = Phase::Value;
                }
            }
            b',' => {
                if let Some(frame) = self.stack.last_mut() {
                    frame.phase = if frame.is_array { Phase::Value } else { Phase::Key };
                }
            }
            _ => {
                self.scalar = Some(Scalar { token: String::new(), slot: self.value_slot() });
                return index;
            }
        }
        index + 1
    }

    /// Where a value starting now goes, or `None` when this position takes no value.
    fn value_slot(&self) -> Option<Slot> {
        if self.dropped_depth > 0 {
            return None;
        }
        let Some(frame) = self.stack.last() else { return self.value.is_none().then_some(Slot::Root) };
        if frame.is_array {
            return Some(Slot::Element(frame.len));
        }
        // A missing colon is tolerated: `{"a" 1}` reads as `{"a": 1}`.
        match frame.phase {
            Phase::Value | Phase::Colon => frame.key.clone().map(Slot::Member),
            Phase::Key | Phase::Comma => None,
        }
    }

    /// The path of a value at `slot` in the innermost open container, when its changes are reported.
    fn reported_path(&self, slot: Option<&Slot>) -> Option<JsonPath> {
        let segment = match slot? {
            Slot::Root => return Some(JsonPath::from([])),
            Slot::Member(key) => PathSegment::Key(key.clone()),
            Slot::Element(index) => PathSegment::Index(*index),
        };
        let frame = self.stack.last().filter(|frame| frame.own.is_none())?;
        Some(frame.path.as_ref()?.iter().cloned().chain([segment]).collect())
    }

    /// Place `value` at `slot`: in `value` and reported when `path` is set, else in the innermost
    /// container's own value. Returns the index of the reported change.
    fn set(&mut self, slot: Option<Slot>, path: Option<JsonPath>, value: Value) -> Option<usize> {
        let slot = slot?;
        if let (Slot::Element(index), Some(frame)) = (&slot, self.stack.last_mut()) {
            frame.len = frame.len.max(index + 1);
        }
        if let Some(path) = path {
            set_at(&mut self.value, &path, value.clone());
            self.changes.push(JsonChange::Set { path, value });
            return Some(self.changes.len() - 1);
        }
        match (self.stack.last_mut().and_then(|frame| frame.own.as_mut()), slot) {
            (Some(Value::Array(items)), Slot::Element(index)) => set_item(items, index, value),
            (Some(Value::Object(members)), Slot::Member(key)) => {
                members.insert(key, value);
            }
            _ => {}
        }
        None
    }

    fn open_container(&mut self, is_array: bool) {
        let slot = self.value_slot();
        if self.dropped_depth > 0 || slot.is_none() || self.stack.len() >= MAX_NESTING {
            self.dropped_depth += 1;
            return;
        }
        let path = self.reported_path(slot.as_ref());
        let empty = if is_array { Value::Array(Vec::new()) } else { Value::Object(Map::new()) };
        let reports_inside = path.as_ref().is_some_and(|path| path.len() < MAX_REPORTED_DEPTH);
        let own = (!reports_inside).then(|| empty.clone());
        if path.is_some() {
            self.set(slot.clone(), path.clone(), empty);
        }
        let phase = if is_array { Phase::Value } else { Phase::Key };
        self.stack.push(Frame { is_array, slot, path, own, len: 0, key: None, phase });
    }

    fn close_container(&mut self) {
        if self.dropped_depth > 0 {
            self.dropped_depth -= 1;
            if self.dropped_depth == 0 {
                self.value_ended();
            }
            return;
        }
        // A stray closer outside any container is ignored.
        let Some(frame) = self.stack.pop() else { return };
        if let Some(own) = frame.own {
            // Reported whole at its path, or handed to the container that holds it.
            self.set(frame.slot, frame.path, own);
        }
        self.value_ended();
    }

    /// A value in the innermost container, or the root value, has ended.
    fn value_ended(&mut self) {
        if self.dropped_depth > 0 {
            return;
        }
        match self.stack.last_mut() {
            Some(frame) => frame.phase = Phase::Comma,
            // Text after the root value is ignored, but only once a root value exists.
            None => self.done = self.value.is_some(),
        }
    }
}
