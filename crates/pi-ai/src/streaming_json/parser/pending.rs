//! Showing the unterminated number or literal at the end of the text in snapshots.

use serde_json::Value;

use super::{Scalar, Slot, StreamingJsonParser};
use crate::streaming_json::scalars::pending_scalar_value;
use crate::streaming_json::{JsonPath, value_at};

// An unterminated number longer than this is not shown until it ends, so `snapshot()` stays cheap.
const MAX_PENDING_SCALAR_LENGTH: usize = 64;

/// Where `snapshot()` placed the unterminated scalar, so the next piece can take it out again.
#[derive(Debug)]
pub(super) enum Overlay {
    Root,
    Element(JsonPath),
    Member(JsonPath, String, Option<Value>),
}

impl StreamingJsonParser {
    /// Show the unterminated scalar at the end of the text in `value` until the next piece.
    pub(super) fn show_pending_scalar(&mut self) {
        let Some(Scalar { token, slot: Some(slot) }) = &self.scalar else { return };
        if token.len() > MAX_PENDING_SCALAR_LENGTH {
            return;
        }
        let Some(pending) = pending_scalar_value(token) else { return };
        if let Slot::Root = slot {
            self.value = Some(pending);
            self.overlay = Some(Overlay::Root);
            return;
        }
        let Some(path) = self.stack.last().filter(|frame| frame.own.is_none()).and_then(|frame| frame.path.clone())
        else {
            return;
        };
        match (value_at(&mut self.value, &path), slot) {
            (Some(Value::Array(items)), Slot::Element(index)) if *index == items.len() => {
                items.push(pending);
                self.overlay = Some(Overlay::Element(path));
            }
            (Some(Value::Object(members)), Slot::Member(key)) => {
                let previous = members.insert(key.clone(), pending);
                self.overlay = Some(Overlay::Member(path, key.clone(), previous));
            }
            _ => {}
        }
    }

    pub(super) fn hide_pending_scalar(&mut self) {
        self.shown = false;
        match self.overlay.take() {
            None => {}
            Some(Overlay::Root) => self.value = None,
            Some(Overlay::Element(path)) => {
                if let Some(Value::Array(items)) = value_at(&mut self.value, &path) {
                    items.pop();
                }
            }
            Some(Overlay::Member(path, key, previous)) => {
                if let Some(Value::Object(members)) = value_at(&mut self.value, &path) {
                    match previous {
                        Some(previous) => members.insert(key, previous),
                        None => members.shift_remove(&key),
                    };
                }
            }
        }
    }
}
