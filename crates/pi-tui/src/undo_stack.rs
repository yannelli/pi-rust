//! Generic undo stack that stores cloned snapshots.

/// Stack of state snapshots. `push` clones the state; `pop` hands the stored snapshot back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoStack<S: Clone> {
    stack: Vec<S>,
}

impl<S: Clone> Default for UndoStack<S> {
    fn default() -> Self {
        Self { stack: Vec::new() }
    }
}

impl<S: Clone> UndoStack<S> {
    /// Creates an empty stack.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pushes a clone of `state`.
    pub fn push(&mut self, state: &S) {
        self.stack.push(state.clone());
    }

    /// Removes and returns the newest snapshot.
    pub fn pop(&mut self) -> Option<S> {
        self.stack.pop()
    }

    /// Removes all snapshots.
    pub fn clear(&mut self) {
        self.stack.clear();
    }

    /// Number of snapshots.
    pub fn len(&self) -> usize {
        self.stack.len()
    }

    /// True when there are no snapshots.
    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_independent_snapshots() {
        let mut stack = UndoStack::new();
        let mut state = vec!["a".to_string()];
        stack.push(&state);
        state.push("b".to_string());
        stack.push(&state);
        state.clear();
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.pop(), Some(vec!["a".to_string(), "b".to_string()]));
        assert_eq!(stack.pop(), Some(vec!["a".to_string()]));
        assert_eq!(stack.pop(), None);
        stack.push(&state);
        stack.clear();
        assert!(stack.is_empty());
    }
}
