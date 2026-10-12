//! Ring buffer for Emacs-style kill and yank.
//!
//! Consecutive kills can accumulate into one entry. Yank pastes the newest entry; yank-pop rotates.

use std::collections::VecDeque;

/// Killed (deleted) text entries, newest last.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KillRing {
    ring: VecDeque<String>,
}

impl KillRing {
    /// Creates an empty kill ring.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds killed text. With `accumulate`, merges into the newest entry instead of adding one:
    /// `prepend` puts the text first (backward deletion), otherwise it is appended (forward deletion).
    pub fn push(&mut self, text: &str, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }
        match self.ring.back_mut() {
            Some(last) if accumulate => {
                if prepend {
                    last.insert_str(0, text);
                } else {
                    last.push_str(text);
                }
            }
            _ => self.ring.push_back(text.to_string()),
        }
    }

    /// Returns the newest entry without changing the ring.
    pub fn peek(&self) -> Option<&str> {
        self.ring.back().map(String::as_str)
    }

    /// Moves the newest entry to the front (for yank-pop cycling).
    pub fn rotate(&mut self) {
        if self.ring.len() > 1 {
            self.ring.rotate_right(1);
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// True when the ring has no entries.
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushes_accumulates_and_rotates() {
        let mut ring = KillRing::new();
        assert!(ring.is_empty());
        assert_eq!(ring.peek(), None);
        ring.push("", false, false);
        assert!(ring.is_empty());

        ring.push("world", false, false);
        ring.push("hello ", true, true);
        assert_eq!(ring.peek(), Some("hello world"));
        ring.push("!", false, true);
        assert_eq!(ring.peek(), Some("hello world!"));
        assert_eq!(ring.len(), 1);

        ring.push("second", false, false);
        ring.push("third", false, false);
        assert_eq!(ring.len(), 3);
        ring.rotate();
        assert_eq!(ring.peek(), Some("second"));
        ring.rotate();
        assert_eq!(ring.peek(), Some("hello world!"));
        ring.rotate();
        assert_eq!(ring.peek(), Some("third"));
    }

    #[test]
    fn accumulate_on_empty_ring_adds_entry() {
        let mut ring = KillRing::new();
        ring.push("x", true, true);
        assert_eq!(ring.peek(), Some("x"));
        ring.rotate();
        assert_eq!(ring.peek(), Some("x"));
    }
}
