//! Durable conversation, task, and document runtime for Pi: a Rust port of `@earendil-works/pi-durable`.
//!
//! Conversations, model turns, tool calls, and application state are committed to storage before anything is shown.

pub mod documents;
pub mod entries;
pub mod env;
pub mod errors;
pub mod harness;
pub mod ids;
pub mod images;
pub mod models;
pub mod session;
pub mod storage;
pub mod tasks;
pub mod testing;
pub mod tools;
pub mod truncate;
pub mod types;
pub mod util;

pub use errors::{DurableError, DurableResult};
pub use ids::{ConversationId, DocumentId, EntryId, ROOT_CONVERSATION_ID, Seq, SubmissionId, TaskId};
pub use types::*;
