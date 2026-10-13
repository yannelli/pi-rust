//! Built-in `Storage` backends: memory (the reference semantics), SQLite, and JSONL (spec section 11).

pub mod memory;
pub mod scan;

pub use memory::{MemoryStorage, PreparedMemoryCommit};
