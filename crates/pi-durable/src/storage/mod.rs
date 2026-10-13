//! Built-in `Storage` backends: memory (the reference semantics), SQLite, and JSONL (spec section 11).

pub mod memory;
pub mod scan;
pub mod sqlite;

pub use memory::{MemoryStorage, PreparedMemoryCommit};
pub(crate) use memory::{is_alive_at, is_current_only};
pub use sqlite::{SqliteStorage, open_sqlite_storage};
