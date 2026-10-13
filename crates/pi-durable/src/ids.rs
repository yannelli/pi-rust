//! Numeric identities of durable records and commit sequences.
//!
//! IDs share one Session-global namespace and stay plain numbers in memory, JSON, JSONL, and SQLite.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! numeric_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl $name {
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl From<$name> for u64 {
            fn from(value: $name) -> u64 {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

numeric_id!(
    /// Identity of one conversation (transcript scope).
    ConversationId
);
numeric_id!(
    /// Identity of one immutable transcript entry.
    EntryId
);
numeric_id!(
    /// Identity of one durable task.
    TaskId
);
numeric_id!(
    /// Identity of one admitted submission.
    SubmissionId
);
numeric_id!(
    /// Identity of one document incarnation; never reused when a logical document is recreated.
    DocumentId
);
numeric_id!(
    /// Strictly increasing sequence of one atomic storage commit. Sequences may skip values.
    Seq
);

/// The root conversation always uses this reserved ID.
pub const ROOT_CONVERSATION_ID: ConversationId = ConversationId(1);
