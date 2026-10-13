//! Built-in `Storage` backends: memory (the reference semantics), SQLite, and JSONL (spec section 11).

pub mod jsonl;
pub mod memory;
pub mod scan;
pub mod sqlite;

pub use jsonl::{JsonlStorage, JsonlStorageOptions, open_local_jsonl_storage};
pub use memory::{MemoryStorage, PreparedMemoryCommit};
pub(crate) use memory::{is_alive_at, is_current_only};
pub use sqlite::{SqliteStorage, open_sqlite_storage};
